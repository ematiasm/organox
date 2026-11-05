//! Authentication use cases.
//!
//! Layer between the HTTP boundary and repository. Responsibilities:
//!  * decide (login policy, session issuance, authorization).
//!  * move expensive argon2 work off the tokio runtime.
//!  * avoid information leaks: the same outcome for "unknown email" and
//!    "incorrect password".

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::OnceLock;
use uuid::Uuid;

use crate::auth::model::{PasswordHash, Role, User};
use crate::auth::password::{self, PasswordError};
use crate::auth::repository::{self, NewUser, RepoError};
use crate::auth::token::{SessionToken, TokenHash};
use crate::auth::validation::{ValidatedEmail, ValidationError};
use crate::ratelimit::{AccountLimiter, Decision};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,

    #[error("the user does not have permission for this operation")]
    Forbidden,

    /// Account temporarily locked by failed attempts.
    /// Deliberately distinct from `InvalidCredentials`: the client needs to
    /// tell the user to wait instead of "incorrect password" (which would lead
    /// them to retry and extend the lock).
    #[error("account temporarily locked until {until}")]
    AccountLocked { until: DateTime<Utc> },

    #[error("the email is already registered")]
    EmailTaken,

    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("internal authentication error")]
    Internal(#[from] RepoError),
}

impl From<sqlx::Error> for AuthError {
    fn from(err: sqlx::Error) -> Self {
        AuthError::Internal(RepoError::Database(err))
    }
}

impl From<PasswordError> for AuthError {
    fn from(err: PasswordError) -> Self {
        match err {
            PasswordError::Invalid(v) => AuthError::Validation(v),
            other => {
                // A hasher failure is a bug or broken dependency, not an invalid
                // credential. Log it and return an internal error.
                tracing::error!(error = %other, "hashing/verification failure");
                AuthError::Internal(RepoError::Crypto)
            }
        }
    }
}

/// Issued session. The plaintext token is delivered **only once**, for the
/// cookie; afterward only its hash exists.
pub struct IssuedSession {
    pub token: SessionToken,
    pub expires_at: DateTime<Utc>,
}

pub enum LoginOutcome {
    Authenticated {
        user: User,
        session: IssuedSession,
    },
    InvalidCredentials,
    /// Locked due to brute force: credentials were not evaluated.
    Locked {
        until: DateTime<Utc>,
    },
}

/// Create a user. Requires the actor to be able to manage users.
///
/// The actor's role is passed explicitly because the authentication extractor,
/// not this module, is the sole authority on "who made this request".
pub async fn create_user(
    pool: &PgPool,
    actor_role: Role,
    email: &str,
    password_plain: &str,
    role: Role,
) -> Result<Uuid, AuthError> {
    if !actor_role.can_manage_users() {
        return Err(AuthError::Forbidden);
    }

    let email = ValidatedEmail::parse(email)?;

    let id = repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash_off_thread(password_plain.to_string()).await?,
            role,
        },
    )
    .await
    .map_err(|err| match err {
        RepoError::EmailTaken => AuthError::EmailTaken,
        other => AuthError::Internal(other),
    })?;

    Ok(id)
}

/// Verify credentials and, if valid, issue a new session.
///
/// Two important details lost in almost all implementations:
///
///  1. If the email does not exist, still spend a hash. Without this, latency
///     distinguishes "unregistered email" from "incorrect password" and login
///     becomes a user enumeration oracle.
///  2. `InvalidCredentials` is the SAME value in both cases.
pub async fn login(
    pool: &PgPool,
    limiter: &AccountLimiter,
    email: &str,
    password_plain: &str,
    user_agent: Option<&str>,
) -> Result<LoginOutcome, AuthError> {
    let Ok(email) = ValidatedEmail::parse(email) else {
        // A malformed email is not a valid credential; do not reveal why.
        return Ok(LoginOutcome::InvalidCredentials);
    };

    // Check the lock BEFORE hashing: if the account is locked, there is no
    // reason to spend 400ms on argon2 or query the user. This also closes the
    // door to using login as a CPU amplifier.
    if let Decision::Blocked { until } = limiter.check(&email).await? {
        return Ok(LoginOutcome::Locked { until });
    }

    let Some(found) = repository::find_user_by_email(pool, &email).await? else {
        // Same cost as a real login, same result. The attempt still counts:
        // otherwise an attacker could distinguish registered emails by the
        // difference between "locked" and "401".
        limiter.record_failure(&email).await?;
        let dummy = dummy_hash();
        let plain = password_plain.to_string();
        let _ = tokio::task::spawn_blocking(move || password::verify(&plain, &dummy)).await;
        return Ok(LoginOutcome::InvalidCredentials);
    };

    let stored = found.password_hash.clone();
    let plain = password_plain.to_string();
    let matches = tokio::task::spawn_blocking(move || password::verify(&plain, &stored))
        .await
        .map_err(|_| AuthError::Internal(RepoError::Crypto))??;

    if !matches {
        // Record the failure, but still return 401: the password WAS incorrect,
        // and telling the user "too many attempts" on the attempt that locked
        // them would be a lie.
        //
        // The lock applies from the NEXT attempt, which returns 429. Thus
        // `max_failures = 3` means 3 incorrect passwords; the fourth is not
        // even evaluated.
        if let Decision::Blocked { until } = limiter.record_failure(&email).await? {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "account locked by failed attempts; applies from the next attempt"
            );
        }
        return Ok(LoginOutcome::InvalidCredentials);
    }

    // Successful login proves this is not an attacker, so clear the count.
    // Without this, someone who made 5 mistakes and then remembered their
    // password would still be locked out.
    limiter.record_success(&email).await?;

    let token = SessionToken::generate();
    let expires_at = repository::create_session(pool, &token.hash(), found.id, user_agent).await?;

    Ok(LoginOutcome::Authenticated {
        user: User {
            id: found.id,
            email: email.as_str().to_string(),
            role: found.role,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
        session: IssuedSession { token, expires_at },
    })
}

/// Resolve a cookie token to an authenticated user.
pub async fn authenticate(
    pool: &PgPool,
    token: &SessionToken,
) -> Result<Option<AuthenticatedUser>, AuthError> {
    let found = repository::find_session_user(pool, &token.hash()).await?;

    Ok(found.map(|u| AuthenticatedUser {
        id: u.id,
        email: u.email,
        role: u.role,
        token_hash: token.hash(),
    }))
}

/// Extend the session used. Best-effort: never fails the request.
pub async fn touch(pool: &PgPool, token: &SessionToken) {
    repository::touch_session(pool, &token.hash()).await;
}

/// End the session. Idempotent: deleting a nonexistent session is not an error.
pub async fn logout(pool: &PgPool, token: &SessionToken) -> Result<(), AuthError> {
    repository::delete_session(pool, &token.hash()).await?;
    Ok(())
}

/// Logout when the token hash is already available (normally from the extractor).
pub async fn logout_by_hash(pool: &PgPool, token_hash: &TokenHash) -> Result<(), AuthError> {
    repository::delete_session(pool, token_hash).await?;
    Ok(())
}

/// User already authenticated in this request. Carries the token hash so
/// logout does not need to reread the cookie.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub token_hash: TokenHash,
}

impl AuthenticatedUser {
    pub fn can_manage_users(&self) -> bool {
        self.role.can_manage_users()
    }
}

async fn hash_off_thread(plain: String) -> Result<PasswordHash, AuthError> {
    tokio::task::spawn_blocking(move || password::hash(&plain))
        .await
        .map_err(|_| AuthError::Internal(RepoError::Crypto))?
        .map_err(AuthError::from)
}

/// Precompute the dummy hash.
///
/// Without this, the first login with an unknown email also pays the cost of
/// generating it, and the latency difference exposes a user enumeration
/// oracle. Called once at server startup.
pub fn warm_dummy_hash() {
    let _ = dummy_hash();
}

/// Dummy hash, computed once per process.
fn dummy_hash() -> PasswordHash {
    static DUMMY: OnceLock<Option<PasswordHash>> = OnceLock::new();

    DUMMY
        .get_or_init(|| password::hash("relleno-solo-para-gastar-tiempo-de-hash").ok())
        .clone()
        .unwrap_or_else(|| {
            // If the hasher is unavailable, any hash is as useless as any
            // other: all that matters is not cutting the path short.
            PasswordHash::from_phc(
                "$argon2id$v=19$m=19456,t=2,p=1$MDAwMDAwMDAwMDAwMDAwMA$0000000000000000000000000000000000000000000"
                    .to_string(),
            )
        })
}
