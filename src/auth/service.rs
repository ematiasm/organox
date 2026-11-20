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
use crate::auth::validation::{self, ValidatedEmail, ValidationError};
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

    #[error("the user does not exist")]
    NotFound,

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

/// Change the calling user's password after checking the current one.
///
/// The new password is validated first: the rule is public, so there is no
/// reason to spend an argon2 round on a request that cannot succeed. The
/// session making the change is spared; the others are revoked in the same
/// transaction as the hash update.
/// What the terminal tool did: the account is new, or its password replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Created(Uuid),
    PasswordReset(Uuid),
}

/// Create a user, or reset the password of an existing one.
///
/// The terminal counterpart of the API, used by `create-user --upsert` so a
/// development database can carry one known account instead of a new one per
/// session. It is still a command someone runs deliberately: there is no boot
/// seed, so nothing resurrects a deleted user or overwrites a password on
/// restart (`docs/auth.md`).
///
/// A reset replaces the hash and revokes every session of that user in one
/// transaction, because a password that no longer exists must not keep a live
/// login alive. The role is left alone: a password reset is not a promotion.
pub async fn create_or_reset_user(
    pool: &PgPool,
    email: &str,
    role: Role,
    password_plain: &str,
) -> Result<UpsertOutcome, AuthError> {
    let email = ValidatedEmail::parse(email)?;
    let new_hash = hash_off_thread(password_plain.to_string()).await?;

    let Some(existing) = repository::find_user_by_email(pool, &email).await? else {
        let id = repository::create_user(
            pool,
            NewUser {
                email: &email,
                password_hash: &new_hash,
                role,
            },
        )
        .await
        .map_err(|err| match err {
            RepoError::EmailTaken => AuthError::EmailTaken,
            other => AuthError::Internal(other),
        })?;
        return Ok(UpsertOutcome::Created(id));
    };

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, existing.id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, existing.id, None).await?;
    tx.commit().await?;

    Ok(UpsertOutcome::PasswordReset(existing.id))
}

pub async fn change_password(
    pool: &PgPool,
    user_id: Uuid,
    current_password: &str,
    new_password: &str,
    token_hash: &TokenHash,
) -> Result<(), AuthError> {
    validation::validate_password(new_password)?;

    let found = repository::find_user_by_id(pool, user_id)
        .await?
        .ok_or(AuthError::NotFound)?;

    let matches = verify_off_thread(current_password.to_string(), found.password_hash).await?;
    if !matches {
        return Err(AuthError::InvalidCredentials);
    }

    let new_hash = hash_off_thread(new_password.to_string()).await?;

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, user_id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, user_id, Some(token_hash)).await?;
    tx.commit().await?;

    Ok(())
}

/// Reset any user's password on behalf of an owner.
///
/// No current password is requested: an owner cannot know another user's.
/// Every session of the target is revoked except the caller's when the target
/// is the caller, so both this route and `change_password` share one rule.
pub async fn reset_password(
    pool: &PgPool,
    actor_role: Role,
    target_id: Uuid,
    new_password: &str,
    caller_id: Uuid,
    caller_token_hash: &TokenHash,
) -> Result<(), AuthError> {
    if !actor_role.can_manage_users() {
        return Err(AuthError::Forbidden);
    }
    validation::validate_password(new_password)?;

    // Check the target before hashing: a 404 must not pay for a new hash.
    repository::find_user_by_id(pool, target_id)
        .await?
        .ok_or(AuthError::NotFound)?;

    let new_hash = hash_off_thread(new_password.to_string()).await?;
    let keep = (caller_id == target_id).then_some(caller_token_hash);

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, target_id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, target_id, keep).await?;
    tx.commit().await?;

    Ok(())
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

async fn verify_off_thread(plain: String, stored: PasswordHash) -> Result<bool, AuthError> {
    tokio::task::spawn_blocking(move || password::verify(&plain, &stored))
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

/// The hash used when the hasher itself fails.
///
/// Well-formed argon2id with the parameters a real hash uses, a salt no
/// generated hash can produce, and a tag no password produces. Any hash would
/// do here; it is fixed so that the fallback never depends on the thing that
/// just failed.
const FALLBACK_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$MDAwMDAwMDAwMDAwMDAwMA$0000000000000000000000000000000000000000000";

/// Dummy hash, computed once per process.
///
/// The hashed value is meaningless on purpose: nothing is ever authenticated
/// against it, it only has to cost as much as a real verification. A literal
/// would put a credential-shaped constant in the source for no gain, and
/// generating it per process means the dummy is not a stable value either.
fn dummy_hash() -> PasswordHash {
    static DUMMY: OnceLock<Option<PasswordHash>> = OnceLock::new();

    DUMMY
        .get_or_init(|| password::hash(&Uuid::now_v7().to_string()).ok())
        .clone()
        .unwrap_or_else(|| {
            // If the hasher is unavailable, any hash is as useless as any
            // other: all that matters is not cutting the path short.
            PasswordHash::from_phc(FALLBACK_HASH.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dummy hash only does its job while it is a well-formed hash that
    /// rejects.
    ///
    /// A malformed one makes `verify` fail immediately instead of costing a
    /// full argon2 round, and the timing difference that reveals which emails
    /// exist comes back silently: the login path discards `verify`'s result on
    /// that branch on purpose.
    ///
    /// In a debug build `PasswordHash::from_phc` panics first, on its
    /// `debug_assert!`. This assertion is what catches the same defect in a
    /// release build, where that assert is compiled out and the bad value
    /// instead reaches `verify`.
    #[test]
    fn dummy_hash_is_well_formed_and_rejects_a_wrong_password() {
        let dummy = dummy_hash();
        // Generated, not written down: a literal here is a hard-coded value
        // handed to a password API, which is the rule this change removes an
        // instance of (`rust/hard-coded-cryptographic-value`).
        let wrong = Uuid::now_v7().to_string();
        assert!(
            matches!(password::verify(&wrong, &dummy), Ok(false)),
            "the dummy must be a valid hash that rejects, so it costs a real round"
        );
    }

    /// One hash per process: `warm_dummy_hash` at startup has to warm the value
    /// the login path later uses. If the input were regenerated per call, the
    /// warm-up would warm nothing and the first unknown-email login would pay
    /// the generation cost, reopening the timing difference the dummy hides.
    #[test]
    fn dummy_hash_is_generated_once_per_process() {
        assert_eq!(dummy_hash().as_str(), dummy_hash().as_str());
    }

    /// The dummy must be a generated hash, never the fixed fallback.
    ///
    /// If the generated input ever stopped passing `validate_password`, every
    /// process would settle silently on `FALLBACK_HASH` — the stable value this
    /// change exists to remove — and the well-formedness test above would still
    /// pass, because the fallback is a valid hash too.
    #[test]
    fn dummy_hash_does_not_settle_for_the_fixed_fallback() {
        assert_ne!(
            dummy_hash().as_str(),
            FALLBACK_HASH,
            "the dummy fell back to the fixed literal"
        );
    }
}
