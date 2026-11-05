//! Authentication data access.
//!
//! All auth SQL lives here. Queries use `sqlx` macros so the compiler checks
//! types and column names against the real DB.
//!
//! Inevitable consequence: compilation requires a reachable DB via
//! `DATABASE_URL`, or the `cargo sqlx prepare` cache committed in `.sqlx/`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{PasswordHash, Role, SESSION_MAX_LIFETIME, SESSION_TTL};
use super::token::TokenHash;
use super::validation::ValidatedEmail;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("the email is already registered")]
    EmailTaken,

    /// Cryptographic failure (hasher unavailable, corrupt hash). This is a bug
    /// or broken dependency, never an invalid credential.
    #[error("cryptographic failure")]
    Crypto,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

fn is_email_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("users_email_unique"))
}

pub struct NewUser<'a> {
    pub email: &'a ValidatedEmail,
    pub password_hash: &'a PasswordHash,
    pub role: Role,
}

pub struct FoundUser {
    pub id: Uuid,
    pub password_hash: PasswordHash,
    pub role: Role,
}

pub struct FoundSession {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
}

/// The id is a UUID v7 generated in the app: time-sortable and unpredictable.
pub async fn create_user(pool: &PgPool, new: NewUser<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO users (id, email, password_hash, role)
        VALUES ($1, $2, $3, $4)
        "#,
        id,
        new.email.as_str(),
        new.password_hash.as_str(),
        new.role as Role,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if is_email_unique_violation(&err) {
            RepoError::EmailTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

pub async fn find_user_by_email(
    pool: &PgPool,
    email: &ValidatedEmail,
) -> Result<Option<FoundUser>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id, password_hash, role AS "role: Role"
        FROM users
        WHERE email = $1
        "#,
        email.as_str()
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| FoundUser {
        id: r.id,
        password_hash: PasswordHash::from_phc(r.password_hash),
        role: r.role,
    }))
}

/// Issue a session. `expires_at` is calculated in the DB to avoid depending on
/// the app clock (which may be out of sync with Postgres).
pub async fn create_session(
    pool: &PgPool,
    token_hash: &TokenHash,
    user_id: Uuid,
    user_agent: Option<&str>,
) -> Result<DateTime<Utc>, RepoError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO sessions (token_hash, user_id, user_agent, expires_at)
        VALUES ($1, $2, $3, now() + make_interval(secs => $4))
        RETURNING expires_at
        "#,
        token_hash.as_bytes(),
        user_id,
        user_agent,
        SESSION_TTL.as_secs_f64(),
    )
    .fetch_one(pool)
    .await?;

    Ok(row.expires_at)
}

/// Resolve token → user in one query. `None` covers the three cases where the
/// client must reauthenticate: nonexistent token, expired session, deleted user.
pub async fn find_session_user(
    pool: &PgPool,
    token_hash: &TokenHash,
) -> Result<Option<FoundSession>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT u.id, u.email::text AS "email!", u.role AS "role: Role"
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1
          AND s.expires_at > now()
        "#,
        token_hash.as_bytes()
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| FoundSession {
        id: r.id,
        email: r.email,
        role: r.role,
    }))
}

/// Extend the session used, respecting the absolute cap from `created_at`:
/// a stolen cookie does not live forever through frequent use.
///
/// Best-effort: a failure is logged and does not affect the request.
pub async fn touch_session(pool: &PgPool, token_hash: &TokenHash) {
    let result = sqlx::query!(
        r#"
        UPDATE sessions
        SET last_used_at = now(),
            expires_at = LEAST(
                now() + make_interval(secs => $2),
                created_at + make_interval(secs => $3)
            )
        WHERE token_hash = $1
          AND expires_at > now()
        "#,
        token_hash.as_bytes(),
        SESSION_TTL.as_secs_f64(),
        SESSION_MAX_LIFETIME.as_secs_f64(),
    )
    .execute(pool)
    .await;

    if let Err(err) = result {
        tracing::warn!(error = %err, "could not extend the session");
    }
}

pub async fn delete_session(pool: &PgPool, token_hash: &TokenHash) -> Result<bool, RepoError> {
    let result = sqlx::query!(
        "DELETE FROM sessions WHERE token_hash = $1",
        token_hash.as_bytes()
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Clean up expired sessions. Returns the number deleted.
pub async fn purge_expired_sessions(pool: &PgPool) -> Result<u64, RepoError> {
    let result = sqlx::query!("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}
