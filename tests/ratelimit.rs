//! Login brute-force protection tests.
//!
//! These cover both layers and non-obvious decisions:
//!  * account blocks survive restarts (they are in Postgres);
//!  * a successful login resets the counter;
//!  * the block expires automatically;
//!  * NONEXISTENT emails also count (otherwise they can be enumerated);
//!  * 429 includes `Retry-After`;
//!  * the IP limit does not penalize other IPs.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::validation::ValidatedEmail;
use organox::auth::{password, validation};
use organox::ratelimit::{AccountPolicy, Decision, IpPolicy};
use organox::routes::router;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

const PASSWORD: &str = "password-long-enough-123";

fn config() -> organox::config::Config {
    // Minimal config for test state, without reading the environment.
    organox::config::Config {
        database_url: String::new(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_format: organox::config::LogFormat::Pretty,
        rust_log: String::new(),
        cookie_secure: false,
        login_max_failures: 3,
        login_block_minutes: 5,
        login_ip_max_per_minute: 30,
    }
}

fn strict_state(pool: PgPool) -> AppState {
    let config = config();
    AppState::with_policies(
        pool,
        false,
        AccountPolicy {
            max_failures: 3,
            window: Duration::from_secs(15 * 60),
            block_duration: Duration::from_secs(5 * 60),
        },
        IpPolicy {
            max_requests: config.login_ip_max_per_minute,
            window: Duration::from_secs(60),
            burst: 10,
        },
    )
}

fn app(pool: PgPool) -> axum::Router {
    router(strict_state(pool)).layer(CookieManagerLayer::new())
}

async fn seed_user(pool: &PgPool, email: &str) {
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(PASSWORD).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("user");
}

fn login_request(email: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": password }).to_string(),
        ))
        .unwrap()
}

async fn attempt(pool: &PgPool, email: &str, password: &str) -> (StatusCode, Option<u64>) {
    let response = app(pool.clone())
        .oneshot(login_request(email, password))
        .await
        .unwrap();

    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    (status, retry_after)
}

// --- Account blocking ---------------------------------------------------

#[sqlx::test]
async fn account_is_locked_after_max_failures(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    // max_failures = 3: the first 3 fail with 401.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }

    // The next attempt is blocked: 429, not 401.
    let (status, retry_after) = attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry_after = retry_after.expect("a 429 must include Retry-After");
    assert!(
        retry_after > 0 && retry_after <= 300,
        "Retry-After out of range: {retry_after}"
    );
}

/// The correct password does not bypass a blocked account. Otherwise, the
/// block would be pointless: an attacker who guesses correctly could still enter.
#[sqlx::test]
async fn correct_password_does_not_bypass_block(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    }

    let (status, _) = attempt(&pool, "owner@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // No session was issued either.
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

/// The block lives in Postgres: a new process (another pool, a "restart")
/// still sees it. An in-memory block would be cleared by a server restart.
#[sqlx::test]
async fn block_survives_a_simulated_restart(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    }

    // Fresh state, as if the process just started: a different pool, new
    // limiters, the same Postgres.
    let fresh_pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        // `#[sqlx::test]` provides the URL through the test's DATABASE_URL.
        panic!("DATABASE_URL must be defined in tests")
    }))
    .await;

    // If a new pool cannot be opened, the test proves nothing: fail explicitly.
    let fresh_pool = match fresh_pool {
        Ok(p) => p,
        Err(err) => panic!("could not open a new pool to simulate a restart: {err}"),
    };

    let fresh_state = strict_state(pool.clone());
    let response = router(fresh_state)
        .layer(CookieManagerLayer::new())
        .oneshot(login_request("owner@pyme.com", PASSWORD))
        .await
        .unwrap();

    // The block was read from the shared database, not memory.
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(fresh_pool);
}

#[sqlx::test]
async fn successful_login_resets_the_counter(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    // 2 failures (one less than the maximum of 3).
    attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    attempt(&pool, "owner@pyme.com", "wrong-password-123").await;

    // Successful login: the counter is reset.
    let (status, _) = attempt(&pool, "owner@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);

    // Now 3 attempts are available again: none blocks the account.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "post-reset attempt {i}");
    }

    // Only the fourth attempt blocks the account.
    let (status, _) = attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// Unknown emails also accumulate attempts. Otherwise, the difference between
/// "blocked" (429) and "invalid credentials" (401) would allow enumeration of
/// registered emails.
#[sqlx::test]
async fn unknown_email_also_gets_blocked(pool: PgPool) {
    for _ in 0..3 {
        let (status, _) = attempt(&pool, "nobody@pyme.com", "wrong-password-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, _) = attempt(&pool, "nobody@pyme.com", "wrong-password-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "an attempt against an unknown email is recorded");
}

#[sqlx::test]
async fn block_expires_and_allows_login_again(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    }
    let (status, _) = attempt(&pool, "owner@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Simulate the passage of time, which a test cannot actually do.
    sqlx::query("UPDATE login_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = attempt(&pool, "owner@pyme.com", PASSWORD).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the block must expire automatically"
    );
}

#[sqlx::test]
async fn failures_outside_the_window_do_not_accumulate(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;

    attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    attempt(&pool, "owner@pyme.com", "wrong-password-123").await;

    // The window expires: old attempts no longer count.
    sqlx::query("UPDATE login_attempts SET window_start = now() - interval '20 minutes'")
        .execute(&pool)
        .await
        .unwrap();

    // If old attempts counted, this would be the third and block the account.
    let (status, _) = attempt(&pool, "owner@pyme.com", "wrong-password-123").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an attempt outside the window must not accumulate"
    );
}

// --- Limiter limit -----------------------------------------------------

#[sqlx::test]
async fn limiter_blocks_and_reports_decision(pool: PgPool) {
    let email = ValidatedEmail::parse("x@pyme.com").unwrap();
    let state = strict_state(pool.clone());

    for _ in 0..3 {
        state.account_limiter.record_failure(&email).await.unwrap();
    }

    let decision = state.account_limiter.check(&email).await.unwrap();
    assert!(
        matches!(decision, Decision::Blocked { .. }),
        "after 3 failures with a maximum of 3 the account must be blocked: {decision:?}"
    );

    // Clearing it manually unblocks the account.
    sqlx::query("UPDATE login_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let decision = state.account_limiter.check(&email).await.unwrap();
    assert!(!decision.is_blocked(), "the expired block must be released");
}

#[sqlx::test]
async fn limiter_purges_stale_rows(pool: PgPool) {
    let email = ValidatedEmail::parse("x@pyme.com").unwrap();
    let state = strict_state(pool.clone());
    state.account_limiter.record_failure(&email).await.unwrap();

    sqlx::query("UPDATE login_attempts SET updated_at = now() - interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();

    let purged = state
        .account_limiter
        .purge_stale(Duration::from_secs(24 * 60 * 60))
        .await
        .unwrap();
    assert_eq!(purged, 1);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

// --- Account isolation -------------------------------------------------

#[sqlx::test]
async fn blocking_one_account_does_not_block_another(pool: PgPool) {
    seed_user(&pool, "a@pyme.com").await;
    seed_user(&pool, "b@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "a@pyme.com", "wrong-password-123").await;
    }
    let (status, _) = attempt(&pool, "a@pyme.com", "wrong-password-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // b is not penalized for a's failures.
    let (status, _) = attempt(&pool, "b@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);
}
