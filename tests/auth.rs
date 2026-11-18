//! Authentication integration tests.
//!
//! Each test runs against an ephemeral database created by `#[sqlx::test]`
//! and exercises the in-memory `Router` without a port or server process.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::routes::router;
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

/// A password generated per test run, so no credential is written down.
fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

/// A short password for the rejection case, under `MIN_PASSWORD_LEN`.
fn short_password() -> String {
    test_password()[..5].to_string()
}

fn app(pool: sqlx::PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
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
