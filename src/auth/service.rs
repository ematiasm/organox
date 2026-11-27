//! Authentication use cases.
//!
//! Layer between the HTTP boundary and repository. Responsibilities:
//!  * decide (login policy, session issuance, authorization).
//!  * move expensive argon2 work off the tokio runtime.
//!  * avoid information leaks: the same outcome for "unknown email" and
//!    "incorrect password".

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Maximum number of concurrent Argon2id evaluations per process.
///
/// Each evaluation allocates ~19 MiB (`m=19456` KiB) and burns CPU for
/// hundreds of milliseconds. Bounding concurrency keeps bursts of login or
/// password-change requests from exhausting memory or saturating Tokio's
/// blocking pool.
pub const MAX_CONCURRENT_HASHES: usize = 4;

use crate::auth::model::{PasswordHash, Role, User};
use crate::auth::password::{self, PasswordError};
pub use crate::auth::repository::ListedUser;
use crate::auth::repository::{self, NewUser, RepoError};
use crate::auth::token::{SessionToken, TokenHash};
use crate::auth::validation::{self, ValidatedEmail, ValidationError};
use crate::ratelimit::{AccountLimiter, AttemptReservation};

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
#[derive(Debug)]
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

/// List every user in email order. Requires the actor to be able to manage users.
///
/// The actor's role is passed explicitly because the authentication extractor,
/// not this module, is the sole authority on "who made this request".
pub async fn list_users(pool: &PgPool, actor_role: Role) -> Result<Vec<ListedUser>, AuthError> {
    if !actor_role.can_manage_users() {
        return Err(AuthError::Forbidden);
    }

    Ok(repository::list_users(pool).await?)
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

    create_user_record(pool, email, password_plain, role).await
}

/// The single creation path, shared by the API route and the terminal upsert.
///
/// Owns everything a new account must satisfy — email and password validation,
/// hashing, the insert, the "already registered" answer — so a change to
/// creation policy lands here and nowhere else.
async fn create_user_record(
    pool: &PgPool,
    email: &str,
    password_plain: &str,
    role: Role,
) -> Result<Uuid, AuthError> {
    let email = ValidatedEmail::parse(email)?;
    validation::validate_password(password_plain)?;

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
/// Creation goes through `create_user_record` — the same path as the API — so
/// creation policy changes land in one place.
pub async fn create_or_reset_user(
    pool: &PgPool,
    email: &str,
    role: Role,
    password_plain: &str,
) -> Result<UpsertOutcome, AuthError> {
    let email = ValidatedEmail::parse(email)?;

    let Some(existing) = repository::find_user_by_email(pool, &email).await? else {
        let id = create_user_record(pool, email.as_str(), password_plain, role).await?;
        return Ok(UpsertOutcome::Created(id));
    };

    // The password policy belongs to the service, not to whoever calls it: a
    // documented minimum that only the CLI enforces is a minimum the next
    // caller forgets.
    validation::validate_password(password_plain)?;
    let new_hash = hash_off_thread(password_plain.to_string()).await?;

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, existing.id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, existing.id, None).await?;
    tx.commit().await?;

    Ok(UpsertOutcome::PasswordReset(existing.id))
}

/// Change the calling user's password after checking the current one.
///
/// The new password is validated first: the rule is public, so there is no
/// reason to burn a rate-limit slot or spend an argon2 round on a request that
/// cannot succeed. Every existing session of the user — including the caller's
/// previous token — is revoked and replaced with a newly issued session in the
/// same transaction as the hash update.
pub async fn change_password(
    pool: &PgPool,
    limiter: &AccountLimiter,
    user_id: Uuid,
    email: &str,
    current_password: &str,
    new_password: &str,
    user_agent: Option<&str>,
) -> Result<IssuedSession, AuthError> {
    validation::validate_password(new_password)?;

    if current_password.len() > validation::MAX_PASSWORD_BYTES {
        // No valid password can exceed `MAX_PASSWORD_BYTES`; reject before
        // touching the database, the Argon2 semaphore, or Argon2 itself.
        return Err(AuthError::InvalidCredentials);
    }

    let email = ValidatedEmail::parse(email).map_err(|_| AuthError::InvalidCredentials)?;

    // Atomically reserve an attempt BEFORE querying the user or running
    // Argon2, matching `login` so a stolen session cannot brute-force
    // `current_password` or amplify Argon2 CPU.
    let locked_until = match limiter.begin_attempt(&email).await? {
        AttemptReservation::Blocked { until } => {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "password change locked due to failed attempts"
            );
            return Err(AuthError::AccountLocked { until });
        }
        AttemptReservation::Allowed { locked_until, .. } => locked_until,
    };

    let found = repository::find_user_by_id(pool, user_id)
        .await?
        .ok_or(AuthError::NotFound)?;

    let matches = verify_off_thread(current_password.to_string(), found.password_hash).await?;
    if !matches {
        if let Some(until) = locked_until {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "account locked by failed password-change attempts; applies from the next attempt"
            );
        }
        return Err(AuthError::InvalidCredentials);
    }

    limiter.record_success(&email).await?;

    let new_hash = hash_off_thread(new_password.to_string()).await?;
    let token = SessionToken::generate();

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, user_id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, user_id, None).await?;
    let expires_at =
        repository::create_session(&mut *tx, &token.hash(), user_id, user_agent).await?;
    tx.commit().await?;

    Ok(IssuedSession { token, expires_at })
}

/// Reset any user's password on behalf of an owner.
///
/// No current password is requested: an owner cannot know another user's.
/// Every existing session of the target is revoked atomically; when the owner
/// resets their own password (`caller_id == target_id`), a fresh session token
/// is issued in the same transaction so the caller's token is rotated rather
/// than preserved.
pub async fn reset_password(
    pool: &PgPool,
    actor_role: Role,
    target_id: Uuid,
    new_password: &str,
    caller_id: Uuid,
    user_agent: Option<&str>,
) -> Result<Option<IssuedSession>, AuthError> {
    if !actor_role.can_manage_users() {
        return Err(AuthError::Forbidden);
    }
    validation::validate_password(new_password)?;

    // Check the target before hashing: a 404 must not pay for a new hash.
    repository::find_user_by_id(pool, target_id)
        .await?
        .ok_or(AuthError::NotFound)?;

    let new_hash = hash_off_thread(new_password.to_string()).await?;

    let mut tx = pool.begin().await?;
    if !repository::update_password_hash(&mut *tx, target_id, &new_hash).await? {
        return Err(AuthError::NotFound);
    }
    repository::delete_sessions_for_user(&mut *tx, target_id, None).await?;
    let rotated = if caller_id == target_id {
        let token = SessionToken::generate();
        let expires_at =
            repository::create_session(&mut *tx, &token.hash(), target_id, user_agent).await?;
        Some(IssuedSession { token, expires_at })
    } else {
        None
    };
    tx.commit().await?;

    Ok(rotated)
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

    if password_plain.len() > validation::MAX_PASSWORD_BYTES {
        // No valid password can exceed `MAX_PASSWORD_BYTES`; reject before
        // touching the database, the Argon2 semaphore, or Argon2 itself.
        return Ok(LoginOutcome::InvalidCredentials);
    }

    // Atomically reserve an attempt BEFORE querying the user or running
    // Argon2: checking first and incrementing after ~390 ms of hashing would
    // let a concurrent burst for the same email bypass `max_failures`.
    // If the account is already locked, no DB user lookup or hash is spent.
    let locked_until = match limiter.begin_attempt(&email).await? {
        AttemptReservation::Blocked { until } => return Ok(LoginOutcome::Locked { until }),
        AttemptReservation::Allowed { locked_until, .. } => locked_until,
    };

    let Some(found) = repository::find_user_by_email(pool, &email).await? else {
        // Same cost as a real login, same result. The attempt was already
        // reserved above: otherwise an attacker could distinguish registered
        // emails by the difference between "locked" and "401".
        if let Some(until) = locked_until {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "account locked by failed attempts; applies from the next attempt"
            );
        }
        let _ = verify_off_thread(password_plain.to_string(), dummy_hash()).await;
        return Ok(LoginOutcome::InvalidCredentials);
    };

    let matches = verify_off_thread(password_plain.to_string(), found.password_hash).await?;

    if !matches {
        // The attempt was already counted in `begin_attempt`, yet we still
        // return 401: the password WAS incorrect, and telling the user "too
        // many attempts" on the attempt that locked them would be a lie.
        //
        // The lock applies from the NEXT attempt, which returns 429. Thus
        // `max_failures = 3` means 3 incorrect passwords; the fourth is not
        // even evaluated.
        if let Some(until) = locked_until {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "account locked by failed attempts; applies from the next attempt"
            );
        }
        return Ok(LoginOutcome::InvalidCredentials);
    }

    // Successful login proves this is not an attacker, so clear the count
    // (including any lockout armed by this attempt). Without this, someone who
    // made 2 mistakes and then remembered their password on attempt 3 would
    // still be locked out.
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

fn argon2_semaphore() -> &'static Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)))
}

async fn run_argon2<T, F>(semaphore: &Arc<Semaphore>, work: F) -> Result<T, AuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PasswordError> + Send + 'static,
{
    let permit = semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AuthError::Internal(RepoError::Crypto))?;

    tokio::task::spawn_blocking(move || {
        // Hold the owned permit inside the blocking task so cancelling the
        // caller's future cannot release the slot while Argon2 is still running.
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| AuthError::Internal(RepoError::Crypto))?
    .map_err(AuthError::from)
}

async fn hash_off_thread(plain: String) -> Result<PasswordHash, AuthError> {
    run_argon2(argon2_semaphore(), move || password::hash(&plain)).await
}

async fn verify_off_thread(plain: String, stored: PasswordHash) -> Result<bool, AuthError> {
    run_argon2(argon2_semaphore(), move || {
        password::verify(&plain, &stored)
    })
    .await
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

    #[tokio::test]
    async fn run_argon2_caps_concurrency_to_semaphore_permits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let semaphore = Arc::new(Semaphore::new(2));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..6 {
            let sem = semaphore.clone();
            let current = in_flight.clone();
            let max_seen = peak.clone();
            handles.push(tokio::spawn(async move {
                run_argon2(&sem, move || {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    current.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .expect("work succeeds");
            }));
        }

        for handle in handles {
            handle.await.expect("task completes");
        }

        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "concurrency must reach but never exceed the semaphore permit limit"
        );
        assert_eq!(semaphore.available_permits(), 2);
    }

    #[tokio::test]
    async fn run_argon2_holds_permit_until_blocking_work_finishes_after_cancel() {
        use std::sync::mpsc;
        use std::time::Duration;

        let semaphore = Arc::new(Semaphore::new(1));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let sem = semaphore.clone();
        let handle = tokio::spawn(async move {
            run_argon2(&sem, move || {
                entered_tx.send(()).expect("signal entry");
                release_rx.recv().expect("wait for release");
                Ok(())
            })
            .await
        });

        // Wait until the blocking thread is actively running with the permit.
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("blocking task entered");
        })
        .await
        .expect("entry wait");

        // Cancel the caller's future (simulating a dropped HTTP request).
        handle.abort();
        let _ = handle.await;

        assert_eq!(
            semaphore.available_permits(),
            0,
            "cancelling the caller must not release the permit while blocking work is still running"
        );

        release_tx.send(()).expect("release blocking work");

        let _permit = tokio::time::timeout(Duration::from_secs(2), semaphore.acquire())
            .await
            .expect("permit must be released once blocking work finishes")
            .expect("semaphore open");
    }

    #[tokio::test]
    async fn run_argon2_releases_permit_when_blocking_work_panics() {
        let semaphore = Arc::new(Semaphore::new(1));

        let err = run_argon2(&semaphore, || -> Result<(), PasswordError> {
            panic!("simulated hashing panic");
        })
        .await
        .expect_err("panic must map to an internal crypto error");

        assert!(matches!(err, AuthError::Internal(RepoError::Crypto)));
        assert_eq!(
            semaphore.available_permits(),
            1,
            "a panic inside spawn_blocking must not leak the semaphore permit"
        );
    }

    #[tokio::test]
    async fn login_and_change_password_reject_oversized_without_db_or_argon2_permit() {
        use std::time::Duration;

        // Drain every Argon2 permit and use an unreachable lazy pool: if either
        // function touches Postgres or the Argon2 semaphore, the call will
        // fail with a connection error or time out waiting for a permit.
        let _permits = argon2_semaphore()
            .acquire_many(MAX_CONCURRENT_HASHES as u32)
            .await
            .expect("acquire all permits");
        let pool =
            PgPool::connect_lazy("postgres://127.0.0.1:1/never_connected").expect("lazy pool");
        let limiter = AccountLimiter::new(pool.clone(), crate::ratelimit::AccountPolicy::default());
        let valid_new = format!("pw-{}", Uuid::now_v7());

        // Test both an ASCII byte-boundary overflow and a multi-byte UTF-8
        // string with fewer than 1024 chars but more than 1024 bytes.
        let ascii_oversized = "a".repeat(validation::MAX_PASSWORD_BYTES + 1);
        let multibyte_oversized = "\u{1F512}".repeat(300);
        assert!(multibyte_oversized.chars().count() < validation::MAX_PASSWORD_BYTES);
        assert!(multibyte_oversized.len() > validation::MAX_PASSWORD_BYTES);

        for oversized in [&ascii_oversized, &multibyte_oversized] {
            let outcome = tokio::time::timeout(
                Duration::from_millis(200),
                login(&pool, &limiter, "owner@pyme.com", oversized, None),
            )
            .await
            .expect("login must reject without waiting for an Argon2 permit")
            .expect("login must return InvalidCredentials, not a DB error");
            assert!(matches!(outcome, LoginOutcome::InvalidCredentials));

            let change_err = tokio::time::timeout(
                Duration::from_millis(200),
                change_password(
                    &pool,
                    &limiter,
                    Uuid::now_v7(),
                    "owner@pyme.com",
                    oversized,
                    &valid_new,
                    None,
                ),
            )
            .await
            .expect("change_password must reject without waiting for an Argon2 permit")
            .expect_err("change_password must reject oversized current_password");
            assert!(matches!(change_err, AuthError::InvalidCredentials));
        }
    }
}
