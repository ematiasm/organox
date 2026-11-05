//! Per-account failed-attempt limit, persisted in Postgres.
//!
//! Why the DB rather than memory: the lock must **survive a restart**. If
//! restarting the server clears the counter, an attacker only needs to wait
//! for a deploy or crash the process to recover their attempts.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::AccountPolicy;
use crate::auth::validation::ValidatedEmail;

/// Account limit state after evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// May attempt.
    Allowed { remaining: i32 },
    /// Locked until the indicated time.
    Blocked { until: DateTime<Utc> },
}

impl Decision {
    pub fn is_blocked(&self) -> bool {
        matches!(self, Decision::Blocked { .. })
    }
}

#[derive(Clone)]
pub struct AccountLimiter {
    pool: PgPool,
    policy: AccountPolicy,
}

impl AccountLimiter {
    pub fn new(pool: PgPool, policy: AccountPolicy) -> Self {
        Self { pool, policy }
    }

    pub fn policy(&self) -> AccountPolicy {
        self.policy
    }

    /// Check whether the account may attempt a login.
    ///
    /// Does not lock or increment: only reads. The increment is performed by
    /// [`record_failure`](Self::record_failure) when an attempt fails.
    pub async fn check(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT failed_count, blocked_until
            FROM login_attempts
            WHERE email = $1
            "#,
            email.as_str()
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            // Never attempted: allowed.
            return Ok(Decision::Allowed {
                remaining: self.policy.max_failures,
            });
        };

        // Active lock: return the expiration time so the client can tell the
        // user how long remains, without revealing anything beyond that
        // account's own lock.
        if let Some(until) = row.blocked_until
            && until > Utc::now()
        {
            return Ok(Decision::Blocked { until });
        }

        // Expired lock: clear the counter here on read so no cleanup job is needed.
        if row.blocked_until.is_some() {
            sqlx::query!(
                r#"
                UPDATE login_attempts
                SET failed_count = 0,
                    blocked_until = NULL,
                    window_start = now(),
                    updated_at = now()
                WHERE email = $1
                "#,
                email.as_str()
            )
            .execute(&self.pool)
            .await?;

            return Ok(Decision::Allowed {
                remaining: self.policy.max_failures,
            });
        }

        Ok(Decision::Allowed {
            remaining: (self.policy.max_failures - row.failed_count).max(0),
        })
    }

    /// Record a failed attempt and lock the account if appropriate.
    ///
    /// This is an atomic `UPSERT`: two concurrent attempts cannot both
    /// "start from zero" and lose each other's count.
    ///
    /// The window resets when `now() > window_start + window`, so old attempts
    /// do not count forever.
    pub async fn record_failure(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let window_secs = self.policy.window.as_secs_f64();
        let block_secs = self.policy.block_duration.as_secs_f64();

        let row = sqlx::query!(
            r#"
            INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
            VALUES ($1, 1, now(), NULL, now())
            ON CONFLICT (email) DO UPDATE
            SET failed_count = CASE
                    -- Expired window: the count starts again.
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                        THEN 1
                    ELSE login_attempts.failed_count + 1
                END,
                window_start = CASE
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                        THEN now()
                    ELSE login_attempts.window_start
                END,
                -- Lock on reaching the maximum. The lock applies even if the
                -- previous one expired: a new attempt renews it only after
                -- failures accumulate again.
                blocked_until = CASE
                    WHEN (CASE
                            WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                                THEN 1
                            ELSE login_attempts.failed_count + 1
                        END) >= $3
                        THEN now() + make_interval(secs => $4)
                    ELSE NULL
                END,
                updated_at = now()
            RETURNING failed_count, blocked_until
            "#,
            email.as_str(),
            window_secs,
            self.policy.max_failures,
            block_secs,
        )
        .fetch_one(&self.pool)
        .await?;

        match row.blocked_until {
            Some(until) if until > Utc::now() => Ok(Decision::Blocked { until }),
            _ => Ok(Decision::Allowed {
                remaining: (self.policy.max_failures - row.failed_count).max(0),
            }),
        }
    }

    /// Clear the lock after a successful login.
    ///
    /// Necessary: without this, a user who made 5 mistakes and then remembered
    /// their password remains locked until timeout. A successful login is
    /// sufficient evidence that this is not an attacker.
    pub async fn record_success(&self, email: &ValidatedEmail) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM login_attempts WHERE email = $1",
            email.as_str()
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete counters for accounts without recent activity.
    ///
    /// Without this, the table grows with every email someone mistyped once.
    /// Returns the number of rows deleted.
    pub async fn purge_stale(&self, older_than: std::time::Duration) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            DELETE FROM login_attempts
            WHERE updated_at < now() - make_interval(secs => $1)
            "#,
            older_than.as_secs_f64()
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DB behavior tests live in tests/ratelimit.rs: they need a real database
    // and `#[sqlx::test]` does not apply to unit tests.

    #[test]
    fn decision_knows_when_blocked() {
        assert!(!Decision::Allowed { remaining: 3 }.is_blocked());
        assert!(Decision::Blocked { until: Utc::now() }.is_blocked());
    }
}
