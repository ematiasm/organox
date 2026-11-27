//! Periodic cleanup of expired sessions and stale login attempt counters.
//!
//! Runs inside the server process on a fixed interval and exits cleanly when
//! the server receives `SIGINT`/`SIGTERM`.

use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::auth::repository;
use crate::ratelimit::{AccountLimiter, AccountPolicy};

/// How often the server purges expired `sessions` and stale `login_attempts`.
pub const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Minimum age before an inactive `login_attempts` row is purged.
pub const MIN_ATTEMPT_RETENTION: Duration = Duration::from_secs(60 * 60 * 24); // 24 hours

/// Retention window for `login_attempts`: at least 24 hours and always longer
/// than `window + block_duration` so active counting windows are never pruned.
pub fn attempt_retention(policy: AccountPolicy) -> Duration {
    MIN_ATTEMPT_RETENTION.max(policy.window.saturating_add(policy.block_duration))
}

/// Counts deleted during one cleanup pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    pub expired_sessions: u64,
    pub stale_attempts: u64,
}

/// Run one cleanup pass over `sessions` and `login_attempts`.
///
/// Both purges run independently so a transient failure on one table does not
/// skip cleaning the other; errors are logged and reported as `0` deleted.
pub async fn run_once(
    pool: &PgPool,
    limiter: &AccountLimiter,
    stale_after: Duration,
) -> CleanupReport {
    let expired_sessions = match repository::purge_expired_sessions(pool).await {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::warn!(error = %err, "failed to purge expired sessions");
            0
        }
    };

    let stale_attempts = match limiter.purge_stale(stale_after).await {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::warn!(error = %err, "failed to purge stale login attempts");
            0
        }
    };

    if expired_sessions > 0 || stale_attempts > 0 {
        tracing::info!(
            expired_sessions,
            stale_attempts,
            "purged expired auth records"
        );
    }

    CleanupReport {
        expired_sessions,
        stale_attempts,
    }
}

/// Spawn the background cleanup loop.
///
/// Runs an initial pass right after startup, then repeats every `interval`
/// until `shutdown` signals `true` (or the sender is dropped).
pub fn spawn(
    pool: PgPool,
    limiter: AccountLimiter,
    interval: Duration,
    stale_after: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if *shutdown.borrow() {
            return;
        }

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    run_once(&pool, &limiter, stale_after).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_retention_respects_floor_and_long_policies() {
        let default_policy = AccountPolicy::default();
        assert_eq!(attempt_retention(default_policy), MIN_ATTEMPT_RETENTION);

        let long_policy = AccountPolicy {
            max_failures: 5,
            window: Duration::from_secs(60 * 60 * 12),
            block_duration: Duration::from_secs(60 * 60 * 24),
        };
        assert_eq!(
            attempt_retention(long_policy),
            Duration::from_secs(60 * 60 * 36)
        );
    }

    #[tokio::test]
    async fn spawn_exits_immediately_when_shutdown_already_triggered() {
        let pool =
            PgPool::connect_lazy("postgres://127.0.0.1:1/never_connected").expect("lazy pool");
        let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
        let (_tx, rx) = watch::channel(true);

        let handle = spawn(
            pool,
            limiter,
            Duration::from_secs(60),
            MIN_ATTEMPT_RETENTION,
            rx,
        );

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("cleanup task must exit immediately on pre-triggered shutdown")
            .expect("cleanup task must not panic");
    }
}
