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

/// Outcome of atomically reserving one login attempt before password
/// verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptReservation {
    /// May proceed to verify credentials. `locked_until` is `Some` when this
    /// attempt reached `max_failures` and armed the lockout for subsequent
    /// attempts (cleared by [`AccountLimiter::record_success`] if valid).
    Allowed {
        remaining: i32,
        locked_until: Option<DateTime<Utc>>,
    },
    /// Already locked before this attempt: do not verify credentials.
    Blocked { until: DateTime<Utc> },
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

    /// Atomically reserve one login attempt BEFORE verifying the password.
    ///
    /// Checking before Argon2 and incrementing afterward creates a TOCTOU
    /// window (~390 ms per hash) where a concurrent burst of logins for the
    /// same email all observe `blocked_until IS NULL` and bypass
    /// `max_failures`. Reserving up front serializes concurrent bursts on the
    /// row's primary key without holding a DB connection across Argon2:
    ///
    /// - Attempts `1..=max_failures` return [`AttemptReservation::Allowed`]
    ///   (the Nth arms `blocked_until` for attempt N+1 and later).
    /// - Any subsequent or concurrent attempt past `max_failures` immediately
    ///   returns [`AttemptReservation::Blocked`] without touching `users` or
    ///   spending an Argon2 round.
    /// - A valid login calls [`record_success`](Self::record_success) to clear
    ///   the row.
    pub async fn begin_attempt(
        &self,
        email: &ValidatedEmail,
    ) -> Result<AttemptReservation, sqlx::Error> {
        let window_secs = self.policy.window.as_secs_f64();
        let block_secs = self.policy.block_duration.as_secs_f64();

        loop {
            let reserved = sqlx::query!(
                r#"
                INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
                VALUES (
                    $1,
                    1,
                    now(),
                    CASE WHEN 1 >= $3 THEN now() + make_interval(secs => $4) ELSE NULL END,
                    now()
                )
                ON CONFLICT (email) DO UPDATE
                SET failed_count = CASE
                        WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                          OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                            THEN 1
                        ELSE login_attempts.failed_count + 1
                    END,
                    window_start = CASE
                        WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                          OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                            THEN now()
                        ELSE login_attempts.window_start
                    END,
                    blocked_until = CASE
                        WHEN (CASE
                                WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                                  OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                                    THEN 1
                                ELSE login_attempts.failed_count + 1
                            END) >= $3
                            THEN now() + make_interval(secs => $4)
                        ELSE NULL
                    END,
                    updated_at = now()
                WHERE login_attempts.blocked_until IS NULL
                   OR login_attempts.blocked_until <= now()
                RETURNING failed_count, blocked_until
                "#,
                email.as_str(),
                window_secs,
                self.policy.max_failures,
                block_secs,
            )
            .fetch_optional(&self.pool)
            .await?;

            if let Some(row) = reserved {
                return Ok(AttemptReservation::Allowed {
                    remaining: (self.policy.max_failures - row.failed_count).max(0),
                    locked_until: row.blocked_until,
                });
            }

            // The conditional UPSERT matched an actively locked row under
            // Postgres row locking. Read `blocked_until` in a fresh snapshot;
            // if the lock expired or `record_success` deleted the row in the
            // meantime, loop and reserve cleanly.
            let locked = sqlx::query!(
                r#"
                SELECT blocked_until AS "blocked_until!"
                FROM login_attempts
                WHERE email = $1
                  AND blocked_until IS NOT NULL
                  AND blocked_until > now()
                "#,
                email.as_str(),
            )
            .fetch_optional(&self.pool)
            .await?;

            if let Some(row) = locked {
                return Ok(AttemptReservation::Blocked {
                    until: row.blocked_until,
                });
            }
        }
    }

    /// Check whether the account may attempt a login.
    ///
    /// Does not increment: only reads (and opportunistically clears an expired
    /// lock, guarded by `blocked_until <= now()` so a stale read can never wipe
    /// a freshly written active lock).
    pub async fn check(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let window_secs = self.policy.window.as_secs_f64();

        loop {
            let row = sqlx::query!(
                r#"
                SELECT
                    failed_count,
                    blocked_until,
                    (blocked_until IS NOT NULL AND blocked_until > now()) AS "is_blocked!",
                    (blocked_until IS NOT NULL AND blocked_until <= now()) AS "lock_expired!",
                    (window_start + make_interval(secs => $2) < now()) AS "window_expired!"
                FROM login_attempts
                WHERE email = $1
                "#,
                email.as_str(),
                window_secs,
            )
            .fetch_optional(&self.pool)
            .await?;

            let Some(row) = row else {
                // Never attempted: allowed.
                return Ok(Decision::Allowed {
                    remaining: self.policy.max_failures,
                });
            };

            // Active lock: return the expiration time so the client can tell
            // the user how long remains, without revealing anything beyond
            // that account's own lock.
            if row.is_blocked
                && let Some(until) = row.blocked_until
            {
                return Ok(Decision::Blocked { until });
            }

            // Expired lock: clear ONLY while `blocked_until <= now()` still
            // holds under the row lock. If another request wrote a fresh active
            // lock concurrently, 0 rows are updated and we re-read live state.
            if row.lock_expired {
                let result = sqlx::query!(
                    r#"
                    UPDATE login_attempts
                    SET failed_count = 0,
                        blocked_until = NULL,
                        window_start = now(),
                        updated_at = now()
                    WHERE email = $1
                      AND blocked_until IS NOT NULL
                      AND blocked_until <= now()
                    "#,
                    email.as_str(),
                )
                .execute(&self.pool)
                .await?;

                if result.rows_affected() == 1 {
                    return Ok(Decision::Allowed {
                        remaining: self.policy.max_failures,
                    });
                }

                continue;
            }

            let remaining = if row.window_expired {
                self.policy.max_failures
            } else {
                (self.policy.max_failures - row.failed_count).max(0)
            };

            return Ok(Decision::Allowed { remaining });
        }
    }

    /// Record a failed attempt and lock the account if appropriate.
    ///
    /// This is an atomic `UPSERT`: two concurrent attempts cannot both
    /// "start from zero" and lose each other's count.
    ///
    /// The window resets when `now() > window_start + window` or after an
    /// expired lock, so old attempts do not count forever.
    pub async fn record_failure(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let window_secs = self.policy.window.as_secs_f64();
        let block_secs = self.policy.block_duration.as_secs_f64();

        let row = sqlx::query!(
            r#"
            INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
            VALUES (
                $1,
                1,
                now(),
                CASE WHEN 1 >= $3 THEN now() + make_interval(secs => $4) ELSE NULL END,
                now()
            )
            ON CONFLICT (email) DO UPDATE
            SET failed_count = CASE
                    -- Expired window or expired lock: the count starts again.
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                      OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                        THEN 1
                    ELSE login_attempts.failed_count + 1
                END,
                window_start = CASE
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                      OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                        THEN now()
                    ELSE login_attempts.window_start
                END,
                -- Preserve an active lock; otherwise arm a new lock on reaching
                -- the maximum.
                blocked_until = CASE
                    WHEN login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until > now()
                        THEN login_attempts.blocked_until
                    WHEN (CASE
                            WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                              OR (login_attempts.blocked_until IS NOT NULL AND login_attempts.blocked_until <= now())
                                THEN 1
                            ELSE login_attempts.failed_count + 1
                        END) >= $3
                        THEN now() + make_interval(secs => $4)
                    ELSE NULL
                END,
                updated_at = now()
            RETURNING
                failed_count,
                blocked_until,
                (blocked_until IS NOT NULL AND blocked_until > now()) AS "is_blocked!"
            "#,
            email.as_str(),
            window_secs,
            self.policy.max_failures,
            block_secs,
        )
        .fetch_one(&self.pool)
        .await?;

        if row.is_blocked
            && let Some(until) = row.blocked_until
        {
            Ok(Decision::Blocked { until })
        } else {
            Ok(Decision::Allowed {
                remaining: (self.policy.max_failures - row.failed_count).max(0),
            })
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
