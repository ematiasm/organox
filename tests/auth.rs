//! Authentication integration tests.
//!
//! Each test runs against an ephemeral database created by `#[sqlx::test]`
//! and exercises the in-memory `Router` without a port or server process.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app, seed_user_with_password, test_password};
use organox::auth::model::Role;
use organox::auth::password::{self, PasswordError};
use organox::auth::repository::{self, NewUser, RepoError};
use organox::auth::service::{self, AuthError, LoginOutcome, UpsertOutcome};
use organox::auth::validation::{self, MAX_PASSWORD_BYTES, ValidationError};
use organox::ratelimit::{AccountLimiter, AccountPolicy};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use uuid::Uuid;

/// A short password for the rejection case, under `MIN_PASSWORD_LEN`.
fn short_password() -> String {
    test_password()[..5].to_string()
}

/// Inserts a user directly into the database, without HTTP. Returns the
/// generated password so a caller that logs in reuses the same value.
async fn seed_user(pool: &sqlx::PgPool, email: &str, role: Role) -> String {
    let password = test_password();
    let email = validation::ValidatedEmail::parse(email).expect("valid email");
    let hash = password::hash(&password).expect("hash");

    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role,
        },
    )
    .await
    .expect("create user");

    password
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 64)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
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

/// The session records the browser, bounded: a header of 4 KB is a header,
/// not a row that grows without limit.
#[sqlx::test]
async fn a_long_user_agent_is_stored_truncated(pool: PgPool) {
    let email = "ua@pyme.com";
    let (_id, password) = seed_user_with_password(&pool, email, Role::Owner).await;
    let long_agent = "agent-".repeat(700);

    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .header("user-agent", &long_agent)
                .body(Body::from(
                    json!({ "email": email, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored: Option<String> = sqlx::query_scalar("SELECT user_agent FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored.unwrap_or_default().chars().count(),
        512,
        "the stored user agent keeps its bound"
    );
}

/// Extracts the session cookie from `Set-Cookie`.
fn session_cookie(response: &axum::response::Response) -> String {
    let header = response
        .headers()
        .get("set-cookie")
        .expect("must emit set-cookie")
        .to_str()
        .unwrap();

    let value = header
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, v)| v.to_string())
        .expect("cookie in name=value format");

    assert!(
        header.contains("HttpOnly"),
        "the session cookie must be HttpOnly: {header}"
    );
    assert!(
        header.contains("SameSite=Strict"),
        "the session cookie must be SameSite=Strict: {header}"
    );

    value
}

// --- Login --------------------------------------------------------------

#[sqlx::test]
async fn login_with_valid_credentials_returns_200_and_cookie(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let app = app(pool);

    let response = app
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let cookie = session_cookie(&response);
    assert!(!cookie.is_empty());

    let body = body_json(response).await;
    assert_eq!(body["user"]["email"], "owner@pyme.com");
    assert_eq!(body["user"]["role"], "owner");
    // The hash is never sent in the response.
    assert!(body["user"].get("password_hash").is_none());
}

#[sqlx::test]
async fn login_is_case_insensitive_on_email(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let app = app(pool);

    let response = app
        .oneshot(login_request("  Owner@PyME.COM ", &password))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// The same 401 for an incorrect password and an unknown email: the client
/// cannot distinguish whether an email is registered.
#[sqlx::test]
async fn login_failures_are_indistinguishable(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let wrong = test_password();

    let wrong_password = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &wrong))
        .await
        .unwrap();

    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    let wrong_password_body = body_json(wrong_password).await;

    let unknown_email = app(pool)
        .oneshot(login_request("nobody@pyme.com", &password))
        .await
        .unwrap();

    assert_eq!(unknown_email.status(), StatusCode::UNAUTHORIZED);
    let unknown_email_body = body_json(unknown_email).await;

    assert_eq!(wrong_password_body, unknown_email_body);
    assert_eq!(unknown_email_body["status"], 401);
}

#[sqlx::test]
async fn login_with_malformed_email_returns_401(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(login_request("not-an-email", &test_password()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn login_rejects_oversized_password_consistently_without_recording_attempts(
    pool: sqlx::PgPool,
) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let oversized = "a".repeat(validation::MAX_PASSWORD_BYTES + 1);

    let existing = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &oversized))
        .await
        .unwrap();
    assert_eq!(existing.status(), StatusCode::UNAUTHORIZED);
    let existing_body = body_json(existing).await;

    let unknown = app(pool.clone())
        .oneshot(login_request("nobody@pyme.com", &oversized))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    let unknown_body = body_json(unknown).await;

    assert_eq!(existing_body, unknown_body);
    assert_eq!(
        count_rows(&pool, "login_attempts").await,
        0,
        "oversized passwords must be rejected before hitting the limiter or Argon2"
    );

    // An 8 KiB password still passes the 16 KiB auth body limit and is rejected
    // by `service::login` with 401 before Argon2 or rate-limit recording.
    let eight_kib = "x".repeat(8 * 1024);
    let large = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &eight_kib))
        .await
        .unwrap();
    assert_eq!(large.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(count_rows(&pool, "login_attempts").await, 0);

    // A 1 MiB login payload exceeds the tighter auth body limit (16 KiB) and is
    // rejected by middleware with 413 Payload Too Large before reaching login.
    let one_mib = "x".repeat(1024 * 1024);
    let huge = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &one_mib))
        .await
        .unwrap();
    assert_eq!(huge.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(count_rows(&pool, "login_attempts").await, 0);

    // Real password still logs in cleanly.
    let ok = app(pool)
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

#[sqlx::test]
async fn login_and_change_password_accept_exact_max_password_bytes(pool: sqlx::PgPool) {
    let max_pw = "a".repeat(validation::MAX_PASSWORD_BYTES);
    let email = validation::ValidatedEmail::parse("max@pyme.com").expect("valid email");
    let hash = password::hash(&max_pw).expect("hash");
    repository::create_user(
        &pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("create user");

    let login = app(pool.clone())
        .oneshot(login_request("max@pyme.com", &max_pw))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = session_cookie(&login);

    let new_max_pw = "b".repeat(validation::MAX_PASSWORD_BYTES);
    let change = app(pool.clone())
        .oneshot(change_password_request(&cookie, &max_pw, &new_max_pw))
        .await
        .unwrap();
    assert_eq!(change.status(), StatusCode::NO_CONTENT);

    let relogin = app(pool)
        .oneshot(login_request("max@pyme.com", &new_max_pw))
        .await
        .unwrap();
    assert_eq!(relogin.status(), StatusCode::OK);
}

// --- Session ------------------------------------------------------------

#[sqlx::test]
async fn me_requires_session_cookie(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn me_returns_authenticated_user(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["email"], "owner@pyme.com");
}

#[sqlx::test]
async fn me_rejects_forged_session_token(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", "session=made-up-nonexistent-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn me_rejects_expired_session(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    // Expire the session directly in the database.
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Logout deletes the database session: a stolen cookie no longer works.
#[sqlx::test]
async fn logout_invalidates_session_server_side(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let cookie_header = format!("session={cookie}");

    let logout = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);

    // The same cookie no longer authenticates.
    let after = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

/// The plaintext token is not persisted: only its SHA-256 is.
#[sqlx::test]
async fn session_token_is_not_stored_in_plaintext(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].len(), 32, "the stored token must be a SHA-256");
    assert_ne!(
        stored[0],
        cookie.as_bytes(),
        "the plaintext token must not be in the database"
    );
}

// --- User creation (RBAC) ----------------------------------------------

fn create_user_request(cookie: &str, email: &str, role: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/users")
        .header("content-type", "application/json")
        .header("cookie", format!("session={cookie}"))
        .body(Body::from(
            json!({ "email": email, "password": password, "role": role }).to_string(),
        ))
        .unwrap()
}

#[sqlx::test]
async fn owner_can_create_user(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(
            &cookie,
            "employee@pyme.com",
            "manager",
            &password,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let role: Role = sqlx::query_scalar("SELECT role FROM users WHERE email = 'employee@pyme.com'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role, Role::Manager);
}

/// RBAC: a manager cannot create users. A 403 has no effects: no user is
/// created and no hash is computed.
#[sqlx::test]
async fn manager_cannot_create_user(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "manager@pyme.com", Role::Manager).await;

    let login = app(pool.clone())
        .oneshot(login_request("manager@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(
            &cookie,
            "other@pyme.com",
            "owner",
            &password,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'other@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "a 403 must leave no effects");
}

#[sqlx::test]
async fn anonymous_cannot_create_user(pool: sqlx::PgPool) {
    let password = test_password();
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": "x@pyme.com", "password": password, "role": "owner" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn duplicate_email_returns_409_and_keeps_single_row(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(
            &cookie,
            "owner@pyme.com",
            "manager",
            &password,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'owner@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn weak_password_is_rejected_without_creating_user(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let weak = short_password();

    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "email": "weak@pyme.com", "password": weak, "role": "manager" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'weak@pyme.com'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn invalid_role_in_payload_is_rejected(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool)
        .oneshot(create_user_request(
            &cookie,
            "x@pyme.com",
            "superadmin",
            &password,
        ))
        .await
        .unwrap();

    // axum rejects the body before it reaches the handler.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// --- User listing -------------------------------------------------------

fn list_users_request(cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri("/api/v1/users");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("session={cookie}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn count_rows(pool: &sqlx::PgPool, table: &str) -> i64 {
    match table {
        "users" => sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .unwrap(),
        "sessions" => sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap(),
        "login_attempts" => sqlx::query_scalar("SELECT count(*) FROM login_attempts")
            .fetch_one(pool)
            .await
            .unwrap(),
        other => panic!("unknown table: {other}"),
    }
}

#[sqlx::test]
async fn list_users_requires_session(pool: sqlx::PgPool) {
    seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(list_users_request(None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
}

#[sqlx::test]
async fn manager_cannot_list_users(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "manager@pyme.com", Role::Manager).await;
    let login = app(pool.clone())
        .oneshot(login_request("manager@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(list_users_request(Some(&cookie)))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
}

#[sqlx::test]
async fn owner_lists_all_users_ordered_by_email(pool: sqlx::PgPool) {
    seed_user(&pool, "zoe@pyme.com", Role::Manager).await;
    seed_user(&pool, "anna@pyme.com", Role::Manager).await;
    let password = seed_user(&pool, "mike@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("mike@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool)
        .oneshot(list_users_request(Some(&cookie)))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let users = body.as_array().expect("a list of users");
    assert_eq!(users.len(), 3);

    let emails: Vec<&str> = users
        .iter()
        .map(|user| user["email"].as_str().expect("email"))
        .collect();
    assert_eq!(emails, ["anna@pyme.com", "mike@pyme.com", "zoe@pyme.com"]);

    let roles: Vec<&str> = users
        .iter()
        .map(|user| user["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["manager", "owner", "manager"]);

    for user in users {
        let fields = user.as_object().expect("a user object");
        assert_eq!(fields.len(), 3, "only id, email and role are public");
        assert!(fields.contains_key("id"));
        assert!(user["id"].as_str().is_some(), "id must be a UUID string");
        assert!(fields.contains_key("email"));
        assert!(fields.contains_key("role"));
    }
}

#[sqlx::test]
async fn service_list_users_enforces_owner_role_and_orders_by_email(pool: sqlx::PgPool) {
    seed_user(&pool, "zoe@pyme.com", Role::Manager).await;
    seed_user(&pool, "anna@pyme.com", Role::Manager).await;
    seed_user(&pool, "mike@pyme.com", Role::Owner).await;

    let err = service::list_users(&pool, Role::Manager)
        .await
        .expect_err("manager must not list users through auth::service");
    assert!(matches!(err, AuthError::Forbidden));

    let users = service::list_users(&pool, Role::Owner)
        .await
        .expect("owner must be able to list users through auth::service");

    let summary: Vec<(&str, Role)> = users.iter().map(|u| (u.email.as_str(), u.role)).collect();
    assert_eq!(
        summary,
        [
            ("anna@pyme.com", Role::Manager),
            ("mike@pyme.com", Role::Owner),
            ("zoe@pyme.com", Role::Manager),
        ]
    );
}

// --- Password rotation -------------------------------------------------

fn change_password_request(cookie: &str, current: &str, new: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/password")
        .header("content-type", "application/json")
        .header("cookie", format!("session={cookie}"))
        .body(Body::from(
            json!({ "current_password": current, "new_password": new }).to_string(),
        ))
        .unwrap()
}

fn reset_password_request(cookie: &str, id: Uuid, new: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/users/{id}/password"))
        .header("content-type", "application/json")
        .header("cookie", format!("session={cookie}"))
        .body(Body::from(json!({ "new_password": new }).to_string()))
        .unwrap()
}

async fn password_hash(pool: &sqlx::PgPool, email: &str) -> String {
    sqlx::query_scalar("SELECT password_hash FROM users WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn user_id(pool: &sqlx::PgPool, email: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn me_status(app: axum::Router, cookie: &str) -> StatusCode {
    app.oneshot(
        Request::builder()
            .uri("/api/v1/auth/me")
            .header("cookie", format!("session={cookie}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[sqlx::test]
async fn self_password_change_rejects_wrong_current_password_without_effects(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let wrong = test_password();
    let new = test_password();
    let hash_before = password_hash(&pool, "owner@pyme.com").await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(change_password_request(&cookie, &wrong, &new))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(response).await["detail"],
        "Invalid credentials or expired session."
    );
    assert_eq!(password_hash(&pool, "owner@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
    // The old password still works and the proposed one was never stored.
    assert_eq!(
        app(pool.clone())
            .oneshot(login_request("owner@pyme.com", &password))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app(pool)
            .oneshot(login_request("owner@pyme.com", &new))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn self_password_change_rejects_oversized_current_password_with_401(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let new = test_password();
    let oversized = "a".repeat(validation::MAX_PASSWORD_BYTES + 1);
    let hash_before = password_hash(&pool, "owner@pyme.com").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(change_password_request(&cookie, &oversized, &new))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(response).await["detail"],
        "Invalid credentials or expired session."
    );
    assert_eq!(password_hash(&pool, "owner@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);

    // The old password still works.
    assert_eq!(
        app(pool)
            .oneshot(login_request("owner@pyme.com", &password))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

/// The public rule is checked before the expensive credential check: with a
/// wrong current password AND a weak new one, the answer is 422.
#[sqlx::test]
async fn self_password_change_validates_new_password_before_current_password(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let hash_before = password_hash(&pool, "owner@pyme.com").await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(change_password_request(
            &cookie,
            &test_password(),
            &short_password(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(response).await["detail"],
        "the password must contain at least 12 characters"
    );
    assert_eq!(password_hash(&pool, "owner@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
}

#[sqlx::test]
async fn self_password_change_rotates_session_and_revokes_all_previous_ones(pool: sqlx::PgPool) {
    let old = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let first = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &old))
        .await
        .unwrap();
    let current = session_cookie(&first);
    let second = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &old))
        .await
        .unwrap();
    let other = session_cookie(&second);
    let new = test_password();
    let hash_before = password_hash(&pool, "owner@pyme.com").await;
    assert_eq!(count_rows(&pool, "sessions").await, 2);

    let response = app(pool.clone())
        .oneshot(change_password_request(&current, &old, &new))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers().get_all("set-cookie").iter().count(),
        1,
        "password change must emit exactly one rotated Set-Cookie header"
    );
    let rotated = session_cookie(&response);
    assert_ne!(
        rotated, current,
        "password change must issue a fresh session token instead of reusing the caller's token"
    );
    assert_ne!(rotated, other);
    assert_ne!(password_hash(&pool, "owner@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "sessions").await, 1);
    assert_eq!(
        me_status(app(pool.clone()), &current).await,
        StatusCode::UNAUTHORIZED,
        "previous caller session token must be revoked after rotation"
    );
    assert_eq!(
        me_status(app(pool.clone()), &other).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        me_status(app(pool.clone()), &rotated).await,
        StatusCode::OK,
        "rotated session token must authenticate"
    );
    assert_eq!(
        app(pool.clone())
            .oneshot(login_request("owner@pyme.com", &old))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app(pool)
            .oneshot(login_request("owner@pyme.com", &new))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
async fn anonymous_cannot_change_password(pool: sqlx::PgPool) {
    seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    // No cookie at all: the route sits behind `AuthUser`, which rejects before
    // the body is read, so neither password can matter.
    let body = json!({
        "current_password": test_password(),
        "new_password": test_password(),
    });
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/password")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
}

#[sqlx::test]
async fn manager_cannot_reset_password(pool: sqlx::PgPool) {
    let manager_password = seed_user(&pool, "manager@pyme.com", Role::Manager).await;
    let target_password = seed_user(&pool, "target@pyme.com", Role::Manager).await;
    let login = app(pool.clone())
        .oneshot(login_request("manager@pyme.com", &manager_password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let target_login = app(pool.clone())
        .oneshot(login_request("target@pyme.com", &target_password))
        .await
        .unwrap();
    let target_cookie = session_cookie(&target_login);
    let target = user_id(&pool, "target@pyme.com").await;
    let hash_before = password_hash(&pool, "target@pyme.com").await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(reset_password_request(&cookie, target, &test_password()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["detail"],
        "You do not have permission to perform this operation."
    );
    assert_eq!(password_hash(&pool, "target@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
    assert_eq!(
        me_status(app(pool.clone()), &target_cookie).await,
        StatusCode::OK
    );
    assert_eq!(
        app(pool)
            .oneshot(login_request("target@pyme.com", &target_password))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
async fn owner_reset_of_unknown_user_returns_404_without_effects(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    seed_user(&pool, "target@pyme.com", Role::Manager).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let target = user_id(&pool, "target@pyme.com").await;
    // Prove the route exists: an unregistered path also returns 404.
    let known = app(pool.clone())
        .oneshot(reset_password_request(&cookie, target, &test_password()))
        .await
        .unwrap();
    assert_eq!(known.status(), StatusCode::NO_CONTENT);

    let hash_before = password_hash(&pool, "target@pyme.com").await;
    let users_before = count_rows(&pool, "users").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    let response = app(pool.clone())
        .oneshot(reset_password_request(
            &cookie,
            Uuid::new_v4(),
            &test_password(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(password_hash(&pool, "target@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "users").await, users_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
}

#[sqlx::test]
async fn owner_reset_rejects_forbidden_password_without_effects(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let target_password = seed_user(&pool, "target@pyme.com", Role::Manager).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let target = user_id(&pool, "target@pyme.com").await;
    let hash_before = password_hash(&pool, "target@pyme.com").await;
    let sessions_before = count_rows(&pool, "sessions").await;

    // `"123456789012"` is on `FORBIDDEN_PASSWORDS` (`src/auth/validation.rs:28`),
    // which is private, so the test cannot take the value from there. It is the
    // subject of the test, not a credential: generating it would delete what
    // this proves. If a scanner flags it, the disposition is `used in tests`.
    let response = app(pool.clone())
        .oneshot(reset_password_request(&cookie, target, "123456789012"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(response).await["detail"],
        "the password is too common"
    );
    assert_eq!(password_hash(&pool, "target@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "sessions").await, sessions_before);
    assert_eq!(
        app(pool)
            .oneshot(login_request("target@pyme.com", &target_password))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
async fn reset_validates_the_password_before_checking_whether_the_user_exists(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    // Both fields are already wrong: an id that does not exist and a password
    // the rule forbids. The answer pins the deliberate order — validate first,
    // because the rule is public and the lookup is not.
    let response = app(pool.clone())
        .oneshot(reset_password_request(
            &cookie,
            Uuid::now_v7(),
            &short_password(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(response).await["detail"],
        "the password must contain at least 12 characters"
    );
}

#[sqlx::test]
async fn owner_reset_revokes_target_sessions_and_allows_new_password(pool: sqlx::PgPool) {
    let owner_password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let target_password = seed_user(&pool, "target@pyme.com", Role::Manager).await;
    let login = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &owner_password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let target_login = app(pool.clone())
        .oneshot(login_request("target@pyme.com", &target_password))
        .await
        .unwrap();
    let target_cookie = session_cookie(&target_login);
    let target = user_id(&pool, "target@pyme.com").await;
    let hash_before = password_hash(&pool, "target@pyme.com").await;
    let new = test_password();
    assert_eq!(count_rows(&pool, "sessions").await, 2);

    let response = app(pool.clone())
        .oneshot(reset_password_request(&cookie, target, &new))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers().get("set-cookie").is_none(),
        "resetting another user's password must not rotate the caller's session cookie"
    );
    assert_ne!(password_hash(&pool, "target@pyme.com").await, hash_before);
    // Only the caller's session survives; the caller is not the target.
    assert_eq!(count_rows(&pool, "sessions").await, 1);
    assert_eq!(me_status(app(pool.clone()), &cookie).await, StatusCode::OK);
    assert_eq!(
        me_status(app(pool.clone()), &target_cookie).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app(pool.clone())
            .oneshot(login_request("target@pyme.com", &target_password))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app(pool)
            .oneshot(login_request("target@pyme.com", &new))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
async fn owner_reset_of_own_password_rotates_session_and_revokes_all_previous_ones(
    pool: sqlx::PgPool,
) {
    let password = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let first = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let current = session_cookie(&first);
    let second = app(pool.clone())
        .oneshot(login_request("owner@pyme.com", &password))
        .await
        .unwrap();
    let other = session_cookie(&second);
    let id = user_id(&pool, "owner@pyme.com").await;
    let hash_before = password_hash(&pool, "owner@pyme.com").await;
    let new = test_password();

    let response = app(pool.clone())
        .oneshot(reset_password_request(&current, id, &new))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers().get_all("set-cookie").iter().count(),
        1,
        "owner self-reset must emit exactly one rotated Set-Cookie header"
    );
    let rotated = session_cookie(&response);
    assert_ne!(
        rotated, current,
        "owner self-reset must rotate the caller's session token"
    );
    assert_ne!(rotated, other);
    assert_ne!(password_hash(&pool, "owner@pyme.com").await, hash_before);
    assert_eq!(count_rows(&pool, "sessions").await, 1);
    assert_eq!(
        me_status(app(pool.clone()), &current).await,
        StatusCode::UNAUTHORIZED,
        "previous caller session token must be revoked after self-reset rotation"
    );
    assert_eq!(
        me_status(app(pool.clone()), &other).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        me_status(app(pool.clone()), &rotated).await,
        StatusCode::OK,
        "rotated session token must authenticate"
    );
    assert_eq!(
        app(pool.clone())
            .oneshot(login_request("owner@pyme.com", &password))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app(pool)
            .oneshot(login_request("owner@pyme.com", &new))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

// --- The terminal tool: create, and reset ---------------------------------

/// `create-user --upsert` is the terminal counterpart of creating a user, and it
/// exists so a development database can carry one known account instead of a new
/// one per session. Creating is the old path; resetting replaces the hash and
/// revokes every session of that user, because a password that no longer exists
/// must not keep a live login alive.
#[sqlx::test]
async fn create_or_reset_creates_a_missing_user(pool: PgPool) {
    // Generated per run: a literal here is a CodeQL hard-coded-credential alert.
    let first = test_password();
    let outcome = service::create_or_reset_user(&pool, "dev@pyme.com", Role::Owner, &first)
        .await
        .expect("upsert creates");
    assert!(matches!(outcome, UpsertOutcome::Created(_)));

    let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
    match service::login(&pool, &limiter, "dev@pyme.com", &first, None)
        .await
        .expect("login")
    {
        LoginOutcome::Authenticated { user, .. } => assert_eq!(user.role, Role::Owner),
        LoginOutcome::InvalidCredentials => panic!("the new account must log in"),
        LoginOutcome::Locked { .. } => panic!("the new account must not be locked"),
    }
}

#[sqlx::test]
async fn create_or_reset_replaces_the_password_and_revokes_sessions(pool: PgPool) {
    let first = test_password();
    let second = test_password();
    let id = service::create_user(&pool, Role::Owner, "dev@pyme.com", &first, Role::Owner)
        .await
        .expect("user");

    // A live session for the old password, which the reset must invalidate.
    let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
    let old_session = match service::login(&pool, &limiter, "dev@pyme.com", &first, None)
        .await
        .expect("login")
    {
        LoginOutcome::Authenticated { session, .. } => session,
        LoginOutcome::InvalidCredentials => panic!("login must succeed"),
        LoginOutcome::Locked { .. } => panic!("the account must not be locked"),
    };

    // Reset with a DIFFERENT role: with Owner in and Owner out, the assertion
    // below would pass even if the reset overwrote the stored role.
    let outcome = service::create_or_reset_user(&pool, "dev@pyme.com", Role::Manager, &second)
        .await
        .expect("upsert resets");
    assert!(matches!(outcome, UpsertOutcome::PasswordReset(uid) if uid == id));

    // The old password is gone, the new one works.
    assert!(matches!(
        service::login(&pool, &limiter, "dev@pyme.com", &first, None)
            .await
            .expect("login is evaluated"),
        LoginOutcome::InvalidCredentials
    ));
    assert!(matches!(
        service::login(&pool, &limiter, "dev@pyme.com", &second, None)
            .await
            .expect("login"),
        LoginOutcome::Authenticated { .. }
    ));

    // And the session minted before the reset is dead.
    let still_valid = service::authenticate(&pool, &old_session.token)
        .await
        .expect("authenticate");
    assert!(
        still_valid.is_none(),
        "a reset must revoke the sessions the old password opened"
    );

    // The role is left alone: a password reset is not a promotion.
    let parsed = validation::ValidatedEmail::parse("dev@pyme.com").expect("email");
    let stored = repository::find_user_by_email(&pool, &parsed)
        .await
        .expect("user")
        .expect("exists");
    assert_eq!(
        stored.role,
        Role::Owner,
        "a password reset must not change the role: it was called with Manager"
    );
}

// --- Pins for the `make dev-user` surface --------------------------------

/// `make dev-user` passes `--upsert` and `--password-stdin` to this binary;
/// if either flag disappears or is renamed, the target breaks at exactly the
/// moment a developer relies on it. `--help` needs no database, so this is a
/// plain test rather than an integration one.
#[test]
fn cli_advertises_the_flags_the_dev_user_target_passes() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_create-user"))
        .arg("--help")
        .output()
        .expect("the compiled create-user binary must run");

    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8(output.stdout).expect("--help output is UTF-8");
    assert!(
        stdout.contains("--upsert"),
        "create-user must advertise --upsert: `make dev-user` passes it"
    );
    assert!(
        stdout.contains("--password-stdin"),
        "create-user must advertise --password-stdin: `make dev-user` passes it"
    );
}

/// The `dev-user` recipe must reference the password as `$$DEV_PASSWORD` and
/// must contain no `$(DEV_PASSWORD)` expansion.
///
/// This pins the fix for a plaintext leak, as a test, because nothing else can
/// see it: `$(VAR)` is expanded by make **into the recipe text** it hands to the
/// shell, so the password would sit in that command line — readable via `ps`
/// by every local user, and printed verbatim by `make -n` — for as long as the
/// argon2 hash takes. `$$VAR` leaves the expansion to the shell, which keeps
/// the value out of the recipe text and out of any log. This is not pedantry:
/// delete the assertion and the leak can return with every other test green.
#[test]
fn dev_user_recipe_keeps_the_password_out_of_the_recipe_text() {
    let makefile = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Makefile"))
        .expect("the Makefile sits next to the crate and must be readable");

    let start = makefile
        .lines()
        .position(|line| line.starts_with("dev-user:"))
        .expect("the Makefile must keep a dev-user target");
    let recipe: Vec<&str> = makefile
        .lines()
        .skip(start + 1)
        .take_while(|line| line.starts_with('\t'))
        .collect();
    assert!(!recipe.is_empty(), "dev-user must have a recipe to inspect");

    assert!(
        recipe.iter().any(|line| line.contains("$$DEV_PASSWORD")),
        "the recipe must read the password via the shell ($$DEV_PASSWORD)"
    );
    assert!(
        recipe.iter().all(|line| !line.contains("$(DEV_PASSWORD)")),
        "make would expand $(DEV_PASSWORD) into the recipe text, leaking the \
         password to `ps` and to `make -n` output"
    );
}

// --- Password length boundary regression probes --------------------------

/// Byte boundary with a multi-byte character straddling the limit: the char
/// count stays far below the limit while the byte count crosses it.
#[test]
fn hash_byte_boundary_with_multibyte_chars() {
    let exact_multibyte = format!("{}é", "a".repeat(MAX_PASSWORD_BYTES - 2));
    assert_eq!(exact_multibyte.len(), MAX_PASSWORD_BYTES);
    let hash = password::hash(&exact_multibyte).expect("exact byte count must hash");
    assert!(password::verify(&exact_multibyte, &hash).unwrap());

    let mut over_multibyte = "a".repeat(MAX_PASSWORD_BYTES - 3);
    over_multibyte.push('\u{1F512}');
    assert!(over_multibyte.chars().count() < MAX_PASSWORD_BYTES);
    assert!(over_multibyte.len() > MAX_PASSWORD_BYTES);
    match password::hash(&over_multibyte) {
        Err(PasswordError::Invalid(ValidationError::PasswordTooLong { max })) => {
            assert_eq!(max, MAX_PASSWORD_BYTES);
        }
        other => panic!("expected PasswordTooLong, got {other:?}"),
    }
}

/// An oversized password must not pay an Argon2 round in `password::verify`.
/// A full round costs hundreds of ms even in debug builds; the short-circuit
/// is microseconds, so a 150 ms budget separates them by a wide margin.
#[test]
fn verify_oversized_short_circuits_before_argon2() {
    let stored = password::hash(&format!("pw-{}", Uuid::new_v4())).expect("hash");
    let oversized = "a".repeat(MAX_PASSWORD_BYTES + 1);

    let start = Instant::now();
    let matched = password::verify(&oversized, &stored).expect("oversized verify must not error");
    let elapsed = start.elapsed();

    assert!(!matched);
    assert!(
        elapsed.as_millis() < 150,
        "oversized verify took {elapsed:?}; it must reject before Argon2"
    );
}

/// The cut in `login`/`change_password` is exactly at MAX_PASSWORD_BYTES and
/// happens before any database access: with an unreachable pool an oversized
/// password still yields InvalidCredentials, while an exact-length password
/// proceeds to the database and fails with a connection error.
#[tokio::test]
async fn login_and_change_password_cut_exactly_at_max_bytes() {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy("postgres://127.0.0.1:1/never_connected")
        .expect("lazy pool");
    let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
    let exact = "a".repeat(MAX_PASSWORD_BYTES);
    let over = format!("{exact}b");
    let new_valid = format!("pw-{}", Uuid::new_v4());

    let outcome = service::login(&pool, &limiter, "owner@pyme.com", &over, None)
        .await
        .expect("oversized login must be InvalidCredentials, not a DB error");
    assert!(matches!(outcome, LoginOutcome::InvalidCredentials));

    let err = match service::login(&pool, &limiter, "owner@pyme.com", &exact, None).await {
        Ok(_) => panic!("exact-length login must reach the database"),
        Err(err) => err,
    };
    assert!(matches!(err, AuthError::Internal(RepoError::Database(_))));

    let err = service::change_password(
        &pool,
        &limiter,
        Uuid::now_v7(),
        "owner@pyme.com",
        &over,
        &new_valid,
        None,
    )
    .await
    .expect_err("oversized current_password must be rejected");
    assert!(matches!(err, AuthError::InvalidCredentials));

    let err = service::change_password(
        &pool,
        &limiter,
        Uuid::now_v7(),
        "owner@pyme.com",
        &exact,
        &new_valid,
        None,
    )
    .await
    .expect_err("exact-length current_password must reach the database");
    assert!(matches!(err, AuthError::Internal(RepoError::Database(_))));
}

// --- Sliding session cookie synchronization (issue #110) ----------------

fn parse_set_cookie(response: &axum::response::Response) -> tower_cookies::Cookie<'static> {
    let header = response
        .headers()
        .get("set-cookie")
        .expect("must emit set-cookie")
        .to_str()
        .unwrap();
    tower_cookies::Cookie::parse(header)
        .expect("valid set-cookie header")
        .into_owned()
}

#[sqlx::test]
async fn fresh_session_does_not_emit_redundant_set_cookie(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "fresh@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("fresh@pyme.com", &password))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let me = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(me.status(), StatusCode::OK);
    assert!(
        me.headers().get("set-cookie").is_none(),
        "a freshly issued session within SESSION_REFRESH_INTERVAL must not churn Set-Cookie"
    );
}

#[sqlx::test]
async fn session_touch_skips_db_update_within_refresh_interval(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "throttled@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("throttled@pyme.com", &password))
        .await
        .unwrap();
    let token = session_cookie(&login);

    // 10 minutes after login is still well within SESSION_REFRESH_INTERVAL (1 hour):
    // authenticated requests must NOT write to `sessions`.
    sqlx::query(
        "UPDATE sessions
         SET created_at   = now() - interval '10 minutes',
             last_used_at = now() - interval '10 minutes',
             expires_at   = now() + interval '7 days' - interval '10 minutes'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (before_last_used, before_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    let within_window = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(within_window.status(), StatusCode::OK);
    assert!(
        within_window.headers().get("set-cookie").is_none(),
        "throttled request must not emit Set-Cookie"
    );

    let (after_last_used, after_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        after_last_used, before_last_used,
        "last_used_at must not be updated within SESSION_REFRESH_INTERVAL"
    );
    assert_eq!(
        after_expires, before_expires,
        "expires_at must not be updated within SESSION_REFRESH_INTERVAL"
    );

    // Once older than SESSION_REFRESH_INTERVAL, the next request refreshes DB + cookie once.
    sqlx::query(
        "UPDATE sessions
         SET created_at   = now() - interval '2 hours',
             last_used_at = now() - interval '2 hours',
             expires_at   = now() + interval '7 days' - interval '2 hours'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (stale_last_used, stale_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    let refreshed = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(refreshed.status(), StatusCode::OK);
    assert_eq!(refreshed.headers().get_all("set-cookie").iter().count(), 1);

    let (updated_last_used, updated_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        updated_last_used > stale_last_used,
        "last_used_at must advance once SESSION_REFRESH_INTERVAL has elapsed"
    );
    assert!(
        updated_expires > stale_expires,
        "expires_at must advance once SESSION_REFRESH_INTERVAL has elapsed"
    );

    // An immediate follow-up request must again skip the DB UPDATE.
    let follow_up = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(follow_up.status(), StatusCode::OK);
    assert!(follow_up.headers().get("set-cookie").is_none());

    let (final_last_used, final_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(final_last_used, updated_last_used);
    assert_eq!(final_expires, updated_expires);
}

#[sqlx::test]
async fn session_touch_reaches_30_day_cap_within_refresh_interval_then_stops_updating(
    pool: sqlx::PgPool,
) {
    let password = seed_user(&pool, "cap-throttle@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("cap-throttle@pyme.com", &password))
        .await
        .unwrap();
    let token = session_cookie(&login);

    // Remaining gap to the 30-day cap is only 30 minutes (< 1h SESSION_REFRESH_INTERVAL):
    // one final refresh advances `expires_at` to `created_at + 30 days`, and later requests
    // must skip DB writes completely.
    sqlx::query(
        "UPDATE sessions
         SET created_at   = now() - interval '23 days' - interval '30 minutes',
             last_used_at = now() - interval '30 minutes',
             expires_at   = (now() - interval '23 days' - interval '30 minutes')
                            + interval '30 days' - interval '30 minutes'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let cap: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at + interval '30 days' FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

    let first = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers().get_all("set-cookie").iter().count(), 1);

    let (at_cap_last_used, at_cap_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(at_cap_expires, cap);

    let second = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second.status(), StatusCode::OK);
    assert!(second.headers().get("set-cookie").is_none());

    let (after_last_used, after_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_used_at, expires_at FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after_last_used, at_cap_last_used);
    assert_eq!(after_expires, at_cap_expires);
}

#[sqlx::test]
async fn sliding_session_refreshes_db_and_cookie_expiry(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "slide@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("slide@pyme.com", &password))
        .await
        .unwrap();
    let token = session_cookie(&login);

    // Simulate 2 days elapsed since login: DB expires_at is now only 5 days out.
    sqlx::query(
        "UPDATE sessions
         SET created_at   = now() - interval '2 days',
             last_used_at = now() - interval '2 days',
             expires_at   = now() + interval '5 days'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let before_expires: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

    let me = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(me.status(), StatusCode::OK);
    assert_eq!(me.headers().get_all("set-cookie").iter().count(), 1);
    let refreshed = parse_set_cookie(&me);
    assert_eq!(refreshed.name(), "session");
    assert_eq!(refreshed.value(), token);
    assert_eq!(refreshed.http_only(), Some(true));
    assert_eq!(
        refreshed.same_site(),
        Some(tower_cookies::cookie::SameSite::Strict)
    );
    assert_eq!(refreshed.path(), Some("/"));

    let db_expires: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        db_expires > before_expires + chrono::Duration::days(1),
        "DB expires_at must slide forward toward now() + 7 days"
    );

    let cookie_expires = refreshed
        .expires_datetime()
        .expect("refreshed cookie must set Expires");
    assert_eq!(
        cookie_expires.unix_timestamp(),
        db_expires.timestamp(),
        "cookie Expires must match the DB sliding expires_at"
    );

    // An immediate follow-up request is already synced and must not emit another Set-Cookie.
    let second = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(
        second.headers().get("set-cookie").is_none(),
        "already-refreshed session must not emit redundant Set-Cookie"
    );
}

#[sqlx::test]
async fn sliding_session_cookie_respects_30_day_absolute_cap(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "cap30@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("cap30@pyme.com", &password))
        .await
        .unwrap();
    let token = session_cookie(&login);

    // Created 25 days ago, currently expiring in 2 days -> cap is created_at + 30d (~5d from now).
    sqlx::query(
        "UPDATE sessions
         SET created_at   = now() - interval '25 days',
             last_used_at = now() - interval '5 days',
             expires_at   = now() + interval '2 days'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let cap: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at + interval '30 days' FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

    let me = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(me.status(), StatusCode::OK);
    assert_eq!(me.headers().get_all("set-cookie").iter().count(), 1);
    let refreshed = parse_set_cookie(&me);
    let cookie_expires = refreshed
        .expires_datetime()
        .expect("refreshed cookie must set Expires");
    assert_eq!(
        cookie_expires.unix_timestamp(),
        cap.timestamp(),
        "cookie Expires must be capped at created_at + 30 days"
    );

    // Once at the 30-day cap, further requests emit no Set-Cookie.
    let second = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(
        second.headers().get("set-cookie").is_none(),
        "session already at 30-day cap must not re-emit Set-Cookie"
    );
}

#[sqlx::test]
async fn logout_and_password_change_on_stale_session_emit_single_set_cookie(pool: sqlx::PgPool) {
    // 1. Logout on a stale session emits only the removal cookie, not a refreshed token.
    let password = seed_user(&pool, "stale-logout@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("stale-logout@pyme.com", &password))
        .await
        .unwrap();
    let token = session_cookie(&login);

    sqlx::query("UPDATE sessions SET expires_at = now() + interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();

    let logout = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header("cookie", format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        logout.headers().get_all("set-cookie").iter().count(),
        1,
        "logout on a stale session must emit exactly one Set-Cookie (removal)"
    );
    let removed = parse_set_cookie(&logout);
    assert_eq!(removed.name(), "session");
    assert_eq!(removed.value(), "");
    assert_eq!(
        me_status(app(pool.clone()), &token).await,
        StatusCode::UNAUTHORIZED
    );

    // 2. Password change on a stale session emits only the rotated cookie.
    let login2 = app(pool.clone())
        .oneshot(login_request("stale-logout@pyme.com", &password))
        .await
        .unwrap();
    let old_token = session_cookie(&login2);
    sqlx::query("UPDATE sessions SET expires_at = now() + interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();

    let new_pw = test_password();
    let changed = app(pool.clone())
        .oneshot(change_password_request(&old_token, &password, &new_pw))
        .await
        .unwrap();

    assert_eq!(changed.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        changed.headers().get_all("set-cookie").iter().count(),
        1,
        "password change on a stale session must emit exactly one Set-Cookie (rotated)"
    );
    let rotated = parse_set_cookie(&changed);
    assert_eq!(rotated.name(), "session");
    assert_ne!(rotated.value(), old_token);
    assert_eq!(
        me_status(app(pool.clone()), &old_token).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(me_status(app(pool), rotated.value()).await, StatusCode::OK);
}

// --- Scheduled auth cleanup (issue #110) --------------------------------

#[sqlx::test]
async fn cleanup_run_once_purges_expired_sessions_and_stale_attempts_only(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "cleanup@pyme.com", Role::Owner).await;
    let active_login = app(pool.clone())
        .oneshot(login_request("cleanup@pyme.com", &password))
        .await
        .unwrap();
    let active_token = session_cookie(&active_login);

    let expired_login = app(pool.clone())
        .oneshot(login_request("cleanup@pyme.com", &password))
        .await
        .unwrap();
    let expired_token = session_cookie(&expired_login);

    // Expire only the second session.
    let expired_hash = organox::auth::token::SessionToken::from_raw(&expired_token).hash();
    sqlx::query(
        "UPDATE sessions SET expires_at = now() - interval '1 minute' WHERE token_hash = $1",
    )
    .bind(expired_hash.as_bytes())
    .execute(&pool)
    .await
    .unwrap();

    // Seed one stale attempt, one fresh attempt, and one stale-updated active lock.
    sqlx::query(
        "INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
         VALUES
           ('stale@pyme.com', 2, now() - interval '2 days', NULL, now() - interval '2 days'),
           ('recent@pyme.com', 1, now() - interval '5 minutes', NULL, now() - interval '5 minutes'),
           ('locked@pyme.com', 5, now() - interval '2 days', now() + interval '10 minutes', now() - interval '2 days')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
    let report = organox::auth::cleanup::run_once(
        &pool,
        &limiter,
        organox::auth::cleanup::attempt_retention(limiter.policy()),
    )
    .await;

    assert_eq!(report.expired_sessions, 1);
    assert_eq!(report.stale_attempts, 1);
    assert_eq!(count_rows(&pool, "sessions").await, 1);
    assert_eq!(
        me_status(app(pool.clone()), &active_token).await,
        StatusCode::OK
    );

    let remaining_attempts: Vec<String> =
        sqlx::query_scalar("SELECT email::text FROM login_attempts ORDER BY email")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining_attempts,
        vec!["locked@pyme.com", "recent@pyme.com"]
    );
}

#[sqlx::test]
async fn cleanup_spawn_runs_initial_pass_and_stops_on_shutdown(pool: sqlx::PgPool) {
    let password = seed_user(&pool, "spawn-cleanup@pyme.com", Role::Owner).await;
    let login = app(pool.clone())
        .oneshot(login_request("spawn-cleanup@pyme.com", &password))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);

    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count_rows(&pool, "sessions").await, 1);

    let limiter = AccountLimiter::new(pool.clone(), AccountPolicy::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = organox::auth::cleanup::spawn(
        pool.clone(),
        limiter,
        Duration::from_secs(60),
        organox::auth::cleanup::MIN_ATTEMPT_RETENTION,
        shutdown_rx,
    );

    // Wait briefly for the initial pass to clear the expired session.
    let deadline = Instant::now() + Duration::from_secs(2);
    while count_rows(&pool, "sessions").await > 0 {
        assert!(
            Instant::now() < deadline,
            "spawned cleanup task must purge expired sessions on startup"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    shutdown_tx.send(true).expect("send shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("cleanup task must terminate promptly on shutdown")
        .expect("cleanup task must exit without panicking");
}
