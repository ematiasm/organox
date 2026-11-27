//! HTTP middleware limit integration tests (request timeout and body size caps).

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::service as auth_service;
use organox::routes::{
    AUTH_BODY_LIMIT_BYTES, DEFAULT_BODY_LIMIT_BYTES, HttpLimits, router, router_with_limits,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn assert_problem_json(response: axum::response::Response, expected: StatusCode) {
    assert_eq!(response.status(), expected);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "middleware rejection must use RFC 7807 application/problem+json"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).expect("valid problem+json body");
    assert_eq!(body["type"], "about:blank");
    assert_eq!(body["status"], expected.as_u16());
    assert!(body["title"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(body["detail"].as_str().is_some_and(|s| !s.is_empty()));
}

async fn login_as_owner(pool: &sqlx::PgPool) -> String {
    let email = "limits-owner@pyme.com";
    let password = format!("pw-{}", Uuid::new_v4());
    auth_service::create_user(pool, Role::Owner, email, &password, Role::Owner)
        .await
        .expect("seed owner");

    let app = router(AppState::new(pool.clone(), false));
    let payload = serde_json::to_vec(&json!({ "email": email, "password": password })).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

/// Requests exceeding the configured timeout abort cleanly with 504 Gateway
/// Timeout in RFC 7807 format and do not permanently wedge the pool.
#[sqlx::test]
async fn request_exceeding_timeout_returns_504_problem_json(pool: sqlx::PgPool) {
    let limits = HttpLimits {
        request_timeout: Duration::from_millis(120),
        ..HttpLimits::default()
    };
    let app = router_with_limits(AppState::new(pool.clone(), false), limits);

    // Hold an exclusive lock on `users` so `POST /api/v1/auth/login` stalls on
    // `find_user_by_email` past the configured request timeout.
    let mut tx = pool.begin().await.expect("begin lock tx");
    sqlx::query("LOCK TABLE users IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .expect("lock users");

    let payload = serde_json::to_vec(&json!({
        "email": "stalled@pyme.com",
        "password": format!("pw-{}", Uuid::new_v4()),
    }))
    .unwrap();

    let started = Instant::now();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        response.headers().get("x-request-id").is_some(),
        "504 timeout must retain x-request-id from outer layers"
    );
    assert_problem_json(response, StatusCode::GATEWAY_TIMEOUT).await;
    assert!(
        elapsed < Duration::from_secs(2),
        "timed-out request must abort promptly, took {elapsed:?}"
    );

    // Release the lock and confirm the pool still serves requests cleanly.
    tx.rollback().await.expect("release lock");
    let ready = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::NO_CONTENT);
}

/// Non-JSON routes must enforce the global body limit via `RequestBodyLimitLayer`.
#[sqlx::test]
async fn global_body_limit_rejects_oversized_non_json_route_with_413(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));
    let oversized_len = DEFAULT_BODY_LIMIT_BYTES + 1;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/does-not-exist")
                .header(header::CONTENT_LENGTH, oversized_len)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_problem_json(response, StatusCode::PAYLOAD_TOO_LARGE).await;
}

/// Streamed JSON bodies (without Content-Length) exceeding the global limit are
/// rejected with 413 problem+json, while non-auth routes remain unaffected by
/// the tighter 16 KiB auth cap.
#[sqlx::test]
async fn global_body_limit_enforced_on_streamed_json_and_auth_cap_stays_scoped(pool: sqlx::PgPool) {
    let cookie = login_as_owner(&pool).await;
    let app = router(AppState::new(pool, false));

    // 1. 32 KiB payload (> AUTH_BODY_LIMIT_BYTES, < DEFAULT_BODY_LIMIT_BYTES)
    //    reaches a non-auth handler instead of hitting 413.
    let medium_notes = "n".repeat(AUTH_BODY_LIMIT_BYTES * 2);
    let medium_payload = serde_json::to_vec(&json!({
        "name": "Scoped Limit Customer",
        "notes": medium_notes,
    }))
    .unwrap();
    assert!(medium_payload.len() > AUTH_BODY_LIMIT_BYTES);
    assert!(medium_payload.len() < DEFAULT_BODY_LIMIT_BYTES);

    let medium_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/customers")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(medium_payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        medium_res.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "non-auth routes must not inherit the tighter auth body limit"
    );

    // 2. > 1 MiB streamed body (no Content-Length) hits the global 413 cap.
    let huge_notes = "n".repeat(DEFAULT_BODY_LIMIT_BYTES + 1024);
    let huge_payload = serde_json::to_vec(&json!({
        "name": "Oversized Customer",
        "notes": huge_notes,
    }))
    .unwrap();

    let huge_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/customers")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(huge_payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(huge_res, StatusCode::PAYLOAD_TOO_LARGE).await;
}

/// Auth routes must enforce the tighter body limit (16 KiB) before reaching
/// handlers, for both Content-Length and streamed (no Content-Length) payloads.
#[sqlx::test]
async fn auth_routes_reject_oversized_body_before_handler(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool.clone(), false));

    let oversized_password = "x".repeat(AUTH_BODY_LIMIT_BYTES + 1024);
    let payload = serde_json::to_vec(&json!({
        "email": "owner@pyme.com",
        "password": oversized_password,
    }))
    .unwrap();
    assert!(payload.len() > AUTH_BODY_LIMIT_BYTES);

    // 1. Login with Content-Length header.
    let with_cl = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, payload.len())
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(with_cl, StatusCode::PAYLOAD_TOO_LARGE).await;

    // 2. Login without Content-Length header (chunked/streamed body).
    let without_cl = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(without_cl, StatusCode::PAYLOAD_TOO_LARGE).await;

    // 3. Password change route with oversized Content-Length is rejected before
    //    auth extraction or password hashing.
    let change_pw = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/password")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, payload.len())
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(change_pw, StatusCode::PAYLOAD_TOO_LARGE).await;

    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .expect("count login_attempts");
    assert_eq!(attempts, 0, "413 rejections must not record login attempts");
}

/// Disabling Axum's implicit 2 MiB `DefaultBodyLimit` ensures `HttpLimits` is
/// the single source of truth even when configured above 2 MiB.
#[sqlx::test]
async fn custom_body_limit_above_axum_default_is_respected(pool: sqlx::PgPool) {
    let cookie = login_as_owner(&pool).await;
    let limits = HttpLimits {
        body_limit_bytes: 3 * 1024 * 1024,
        ..HttpLimits::default()
    };
    let app = router_with_limits(AppState::new(pool, false), limits);

    // 2.1 MiB exceeds Axum's built-in 2 MiB DefaultBodyLimit but fits inside 3 MiB.
    let large_notes = "a".repeat(2_200_000);
    let payload = serde_json::to_vec(&json!({
        "name": "Custom Limit Customer",
        "notes": large_notes,
    }))
    .unwrap();
    assert!(payload.len() > 2 * 1024 * 1024);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/customers")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "HttpLimits must control the body limit without Axum silently clamping at 2 MiB"
    );
}

/// Middleware rejections inside `routes::router_with_limits` pass through the
/// outer request-id layers assembled in `routes`, preserving `x-request-id`.
#[sqlx::test]
async fn request_id_is_preserved_on_413_rejection(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/does-not-exist")
                .header(header::CONTENT_LENGTH, DEFAULT_BODY_LIMIT_BYTES + 1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response.headers().get("x-request-id").is_some(),
        "413 rejection must retain x-request-id from outer layers"
    );
    assert_problem_json(response, StatusCode::PAYLOAD_TOO_LARGE).await;
}
