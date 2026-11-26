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
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::validation::ValidatedEmail;
use organox::auth::{password, validation};
use organox::ratelimit::{AccountPolicy, Decision, IpLimiter, IpPolicy};
use organox::routes::router;
use serde_json::json;
use sqlx::PgPool;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

/// A password generated per test run, so no credential is written down.
fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

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

/// Returns the generated password so a caller that logs in reuses it.
async fn seed_user(pool: &PgPool, email: &str) -> String {
    let password = test_password();
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(&password).expect("hash");
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
    password
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
    let wrong = test_password();

    // max_failures = 3: the first 3 fail with 401.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "owner@pyme.com", &wrong).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }

    // The next attempt is blocked: 429, not 401.
    let (status, retry_after) = attempt(&pool, "owner@pyme.com", &wrong).await;
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
    let password = seed_user(&pool, "owner@pyme.com").await;
    let wrong = test_password();

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", &wrong).await;
    }

    let (status, _) = attempt(&pool, "owner@pyme.com", &password).await;
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
    let password = seed_user(&pool, "owner@pyme.com").await;
    let wrong = test_password();

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", &wrong).await;
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
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();

    // The block was read from the shared database, not memory.
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(fresh_pool);
}

#[sqlx::test]
async fn successful_login_resets_the_counter(pool: PgPool) {
    let password = seed_user(&pool, "owner@pyme.com").await;
    let wrong = test_password();

    // 2 failures (one less than the maximum of 3).
    attempt(&pool, "owner@pyme.com", &wrong).await;
    attempt(&pool, "owner@pyme.com", &wrong).await;

    // Successful login: the counter is reset.
    let (status, _) = attempt(&pool, "owner@pyme.com", &password).await;
    assert_eq!(status, StatusCode::OK);

    // Now 3 attempts are available again: none blocks the account.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "owner@pyme.com", &wrong).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "post-reset attempt {i}");
    }

    // Only the fourth attempt blocks the account.
    let (status, _) = attempt(&pool, "owner@pyme.com", &wrong).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// Unknown emails also accumulate attempts. Otherwise, the difference between
/// "blocked" (429) and "invalid credentials" (401) would allow enumeration of
/// registered emails.
#[sqlx::test]
async fn unknown_email_also_gets_blocked(pool: PgPool) {
    let wrong = test_password();
    for _ in 0..3 {
        let (status, _) = attempt(&pool, "nobody@pyme.com", &wrong).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, _) = attempt(&pool, "nobody@pyme.com", &wrong).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "an attempt against an unknown email is recorded");
}

#[sqlx::test]
async fn block_expires_and_allows_login_again(pool: PgPool) {
    let password = seed_user(&pool, "owner@pyme.com").await;
    let wrong = test_password();

    for _ in 0..3 {
        attempt(&pool, "owner@pyme.com", &wrong).await;
    }
    let (status, _) = attempt(&pool, "owner@pyme.com", &password).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Simulate the passage of time, which a test cannot actually do.
    sqlx::query("UPDATE login_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = attempt(&pool, "owner@pyme.com", &password).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the block must expire automatically"
    );
}

#[sqlx::test]
async fn failures_outside_the_window_do_not_accumulate(pool: PgPool) {
    seed_user(&pool, "owner@pyme.com").await;
    let wrong = test_password();

    attempt(&pool, "owner@pyme.com", &wrong).await;
    attempt(&pool, "owner@pyme.com", &wrong).await;

    // The window expires: old attempts no longer count.
    sqlx::query("UPDATE login_attempts SET window_start = now() - interval '20 minutes'")
        .execute(&pool)
        .await
        .unwrap();

    // If old attempts counted, this would be the third and block the account.
    let (status, _) = attempt(&pool, "owner@pyme.com", &wrong).await;
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
    let password_b = seed_user(&pool, "b@pyme.com").await;
    let wrong = test_password();

    for _ in 0..3 {
        attempt(&pool, "a@pyme.com", &wrong).await;
    }
    let (status, _) = attempt(&pool, "a@pyme.com", &wrong).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // b is not penalized for a's failures.
    let (status, _) = attempt(&pool, "b@pyme.com", &password_b).await;
    assert_eq!(status, StatusCode::OK);
}

// --- Concurrency and edge-case regressions (issue #98) -----------------

fn state_with_max_failures(pool: PgPool, max_failures: i32) -> AppState {
    AppState::with_policies(
        pool,
        false,
        AccountPolicy {
            max_failures,
            window: Duration::from_secs(15 * 60),
            block_duration: Duration::from_secs(5 * 60),
        },
        IpPolicy {
            max_requests: 100,
            window: Duration::from_secs(60),
            burst: 100,
        },
    )
}

/// `APP_LOGIN_MAX_FAILURES=1` must lock on the very first failed login,
/// while still allowing a valid first login without leaving a lock behind.
#[sqlx::test]
async fn max_failures_one_locks_on_first_failure(pool: PgPool) {
    let password = seed_user(&pool, "one@pyme.com").await;
    let wrong = test_password();
    let app = router(state_with_max_failures(pool.clone(), 1)).layer(CookieManagerLayer::new());

    // A valid first login succeeds and clears any attempt state.
    let ok = app
        .clone()
        .oneshot(login_request("one@pyme.com", &password))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    // First failed login returns 401 and immediately arms the lockout.
    let first_fail = app
        .clone()
        .oneshot(login_request("one@pyme.com", &wrong))
        .await
        .unwrap();
    assert_eq!(first_fail.status(), StatusCode::UNAUTHORIZED);

    // Second attempt is blocked with 429 + Retry-After (even with valid password).
    let second = app
        .oneshot(login_request("one@pyme.com", &password))
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "max_failures=1 must block the second attempt after a single failure"
    );
    assert!(
        second.headers().get("retry-after").is_some(),
        "429 must include Retry-After"
    );

    // Direct AccountLimiter::record_failure also arms the lock on the first insert.
    let email = ValidatedEmail::parse("direct-one@pyme.com").unwrap();
    let limiter = state_with_max_failures(pool, 1).account_limiter;
    let after_one = limiter.record_failure(&email).await.unwrap();
    assert!(
        after_one.is_blocked(),
        "record_failure with max_failures=1 must return Blocked on first insert, got: {after_one:?}"
    );
    assert!(
        limiter.check(&email).await.unwrap().is_blocked(),
        "check must observe the active lock immediately after 1 failure"
    );
}

/// Deterministic proof of the expired-lock wipe race:
///
/// 1. Row starts with an expired lock (`blocked_until < now()`).
/// 2. Tx A locks the row (`FOR UPDATE`) and writes a fresh active lock
///    (`blocked_until = now() + 5 min`), without committing yet.
/// 3. `AccountLimiter::check` reads the committed expired row under MVCC, then
///    blocks on its reset `UPDATE` waiting for Tx A.
/// 4. Tx A commits the fresh active lock.
/// 5. `check`'s reset `UPDATE` must NOT wipe the fresh active lock and `check`
///    must report `Decision::Blocked`.
#[sqlx::test]
async fn expired_lock_reset_cannot_wipe_fresh_active_lock(pool: PgPool) {
    let email = ValidatedEmail::parse("race-wipe@pyme.com").unwrap();
    let limiter = strict_state(pool.clone()).account_limiter;

    // Seed an expired lock row.
    sqlx::query(
        "INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
         VALUES ($1, 3, now() - interval '6 minutes', now() - interval '1 second', now() - interval '6 minutes')",
    )
    .bind(email.as_str())
    .execute(&pool)
    .await
    .unwrap();

    // Tx A holds the row lock and installs a fresh active lock (uncommitted).
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE login_attempts
         SET failed_count = 3,
             window_start = now(),
             blocked_until = now() + interval '5 minutes',
             updated_at = now()
         WHERE email = $1",
    )
    .bind(email.as_str())
    .execute(&mut *tx)
    .await
    .unwrap();

    // Start `check()` concurrently: its SELECT sees the old expired lock, then
    // its UPDATE blocks waiting for `tx` to commit.
    let check_limiter = limiter.clone();
    let check_email = email.clone();
    let check_task = tokio::spawn(async move { check_limiter.check(&check_email).await });

    // Give `check_task` time to complete its SELECT and block on the row lock.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Commit the fresh active lock, releasing `check_task`'s UPDATE.
    tx.commit().await.unwrap();

    let decision = check_task.await.unwrap().unwrap();
    assert!(
        decision.is_blocked(),
        "stale check must observe the newly committed active lock, got: {decision:?}"
    );

    // And the row in Postgres must STILL be actively locked.
    let still_blocked: bool = sqlx::query_scalar(
        "SELECT blocked_until IS NOT NULL AND blocked_until > now()
         FROM login_attempts WHERE email = $1",
    )
    .bind(email.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        still_blocked,
        "expired-lock reset must not wipe a freshly written active lock"
    );
}

/// Concurrent burst of bad-password logins for the same email must not bypass
/// `max_failures`: with `max_failures = 3` and 8 concurrent requests, exactly
/// 3 are evaluated (401) and 5 are blocked immediately (429).
#[sqlx::test]
async fn concurrent_login_burst_cannot_bypass_max_failures(pool: PgPool) {
    seed_user(&pool, "burst@pyme.com").await;
    let state = state_with_max_failures(pool.clone(), 3);
    let app = router(state).layer(CookieManagerLayer::new());

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let app = app.clone();
        let wrong = test_password();
        set.spawn(async move {
            app.oneshot(login_request("burst@pyme.com", &wrong))
                .await
                .unwrap()
                .status()
        });
    }

    let mut unauthorized = 0;
    let mut too_many = 0;
    while let Some(res) = set.join_next().await {
        match res.unwrap() {
            StatusCode::UNAUTHORIZED => unauthorized += 1,
            StatusCode::TOO_MANY_REQUESTS => too_many += 1,
            other => panic!("unexpected status in concurrent burst: {other}"),
        }
    }

    assert_eq!(
        unauthorized, 3,
        "at most max_failures (3) attempts may reach password verification"
    );
    assert_eq!(
        too_many, 5,
        "the remaining 5 concurrent attempts must be rejected with 429"
    );
}

/// Same concurrent burst guarantee for nonexistent emails (no enumeration oracle).
#[sqlx::test]
async fn concurrent_burst_on_unknown_email_respects_max_failures(pool: PgPool) {
    let state = state_with_max_failures(pool, 3);
    let app = router(state).layer(CookieManagerLayer::new());

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let app = app.clone();
        let wrong = test_password();
        set.spawn(async move {
            app.oneshot(login_request("ghost-burst@pyme.com", &wrong))
                .await
                .unwrap()
                .status()
        });
    }

    let mut unauthorized = 0;
    let mut too_many = 0;
    while let Some(res) = set.join_next().await {
        match res.unwrap() {
            StatusCode::UNAUTHORIZED => unauthorized += 1,
            StatusCode::TOO_MANY_REQUESTS => too_many += 1,
            other => panic!("unexpected status in unknown-email burst: {other}"),
        }
    }

    assert_eq!(unauthorized, 3);
    assert_eq!(too_many, 5);
}

/// After a lock expires (even while still inside the 15-minute counting
/// window), a direct `record_failure` must start a fresh count at 1 rather
/// than immediately re-locking the account.
#[sqlx::test]
async fn record_failure_after_expired_lock_starts_fresh_window(pool: PgPool) {
    let email = ValidatedEmail::parse("expired-direct@pyme.com").unwrap();
    let limiter = strict_state(pool.clone()).account_limiter;

    sqlx::query(
        "INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
         VALUES ($1, 3, now() - interval '6 minutes', now() - interval '1 second', now() - interval '6 minutes')",
    )
    .bind(email.as_str())
    .execute(&pool)
    .await
    .unwrap();

    let decision = limiter.record_failure(&email).await.unwrap();
    assert_eq!(
        decision,
        Decision::Allowed { remaining: 2 },
        "first failure after an expired lock must start a fresh count at 1"
    );
}

// --- IP limiter IPv6 /64 normalization and strict cap (issue #99) ------

fn login_request_from(ip: IpAddr, email: &str, password: &str) -> Request<Body> {
    let mut req = login_request(email, password);
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(ip, 40_000)));
    req
}

fn state_with_ip_limiter(pool: PgPool, ip_limiter: IpLimiter) -> AppState {
    let mut state = state_with_max_failures(pool, 100);
    state.ip_limiter = Arc::new(ip_limiter);
    state
}

#[sqlx::test]
async fn ip_limiter_groups_ipv6_hosts_by_64_prefix_via_http(pool: PgPool) {
    let limiter = IpLimiter::new(IpPolicy {
        max_requests: 2,
        window: Duration::from_secs(60),
        burst: 0,
    });
    let app = router(state_with_ip_limiter(pool, limiter)).layer(CookieManagerLayer::new());
    let wrong = test_password();

    // Two distinct hosts in the same /64 consume the shared quota (2).
    let r1 = app
        .clone()
        .oneshot(login_request_from(
            "2001:db8:1:2::1".parse().unwrap(),
            "ipv6@pyme.com",
            &wrong,
        ))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::UNAUTHORIZED);

    let r2 = app
        .clone()
        .oneshot(login_request_from(
            "2001:db8:1:2::2".parse().unwrap(),
            "ipv6@pyme.com",
            &wrong,
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::UNAUTHORIZED);

    // Third host in the same /64 is rejected with 429 + Retry-After.
    let r3 = app
        .clone()
        .oneshot(login_request_from(
            "2001:db8:1:2:dead:beef::3".parse().unwrap(),
            "ipv6@pyme.com",
            &wrong,
        ))
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        r3.headers().get("retry-after").is_some(),
        "429 from IP limiter must include Retry-After"
    );

    // A different /64 prefix still gets its own quota.
    let r4 = app
        .oneshot(login_request_from(
            "2001:db8:1:3::1".parse().unwrap(),
            "ipv6@pyme.com",
            &wrong,
        ))
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn ip_limiter_fails_closed_at_max_entries_via_http(pool: PgPool) {
    let limiter = IpLimiter::with_max_entries(
        IpPolicy {
            max_requests: 2,
            window: Duration::from_secs(60),
            burst: 0,
        },
        2,
    );
    let app = router(state_with_ip_limiter(pool, limiter)).layer(CookieManagerLayer::new());
    let wrong = test_password();

    let ip1: IpAddr = "198.51.100.1".parse().unwrap();
    let ip2: IpAddr = "198.51.100.2".parse().unwrap();
    let ip3: IpAddr = "198.51.100.3".parse().unwrap();

    // Fill the 2-bucket cap with ip1 and ip2.
    assert_eq!(
        app.clone()
            .oneshot(login_request_from(ip1, "cap@pyme.com", &wrong))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(login_request_from(ip2, "cap@pyme.com", &wrong))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // A 3rd distinct IP fails closed with 429 + Retry-After.
    let blocked = app
        .clone()
        .oneshot(login_request_from(ip3, "cap@pyme.com", &wrong))
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(blocked.headers().get("retry-after").is_some());

    // Already-tracked ip1 still gets its 2nd allowed attempt before hitting 429.
    assert_eq!(
        app.clone()
            .oneshot(login_request_from(ip1, "cap@pyme.com", &wrong))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.oneshot(login_request_from(ip1, "cap@pyme.com", &wrong))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test]
async fn ip_limiter_unwraps_ipv4_mapped_ipv6_via_http(pool: PgPool) {
    let limiter = IpLimiter::new(IpPolicy {
        max_requests: 1,
        window: Duration::from_secs(60),
        burst: 0,
    });
    let app = router(state_with_ip_limiter(pool, limiter)).layer(CookieManagerLayer::new());
    let wrong = test_password();

    // IPv4 consumes the 1-request quota.
    assert_eq!(
        app.clone()
            .oneshot(login_request_from(
                "203.0.113.10".parse().unwrap(),
                "mapped@pyme.com",
                &wrong,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // IPv4-mapped IPv6 for the same IPv4 hits the shared bucket (429).
    assert_eq!(
        app.clone()
            .oneshot(login_request_from(
                "::ffff:203.0.113.10".parse().unwrap(),
                "mapped@pyme.com",
                &wrong,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    // Different IPv4-mapped IPv6 gets its own bucket instead of collapsing to ::/64.
    assert_eq!(
        app.oneshot(login_request_from(
            "::ffff:203.0.113.11".parse().unwrap(),
            "mapped@pyme.com",
            &wrong,
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn ip_rate_limit_returns_rfc7807_problem_json_with_retry_after(pool: PgPool) {
    let limiter = IpLimiter::new(IpPolicy {
        max_requests: 1,
        window: Duration::from_secs(60),
        burst: 0,
    });
    let app = router(state_with_ip_limiter(pool, limiter)).layer(CookieManagerLayer::new());
    let wrong = test_password();
    let ip: IpAddr = "198.51.100.77".parse().unwrap();

    let first = app
        .clone()
        .oneshot(login_request_from(ip, "problem@pyme.com", &wrong))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::UNAUTHORIZED);

    let blocked = app
        .oneshot(login_request_from(ip, "problem@pyme.com", &wrong))
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        blocked
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "IP rate limit 429 must return RFC 7807 application/problem+json"
    );
    let retry_after: u64 = blocked
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .expect("429 from IP limiter must include Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after >= 1);

    let bytes = axum::body::to_bytes(blocked.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["type"], "about:blank");
    assert_eq!(body["title"], "Too Many Requests");
    assert_eq!(body["status"], 429);
    assert!(body["detail"].as_str().is_some_and(|s| !s.is_empty()));
}
