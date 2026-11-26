//! RFC 7807 `application/problem+json` contract tests for HTTP extractor
//! rejections (`AppJson`, `AppPath`, `AppQuery`) across API routes.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::service as auth_service;
use organox::routes::router;
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

async fn assert_problem_json(
    response: axum::response::Response,
    expected_status: StatusCode,
    label: &str,
) {
    assert_eq!(response.status(), expected_status, "{label}");
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "{label}: expected application/problem+json"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{label}: invalid JSON body: {e}"));
    assert_eq!(body["type"], "about:blank", "{label}");
    assert_eq!(body["status"], expected_status.as_u16(), "{label}");
    assert!(
        body["title"].as_str().is_some_and(|s| !s.is_empty()),
        "{label}: title must be non-empty"
    );
    assert!(
        body["detail"].as_str().is_some_and(|s| !s.is_empty()),
        "{label}: detail must be non-empty"
    );
}

async fn login_as_owner(pool: &sqlx::PgPool) -> String {
    let email = "http-errors-owner@pyme.com";
    let password = format!("pw-{}", Uuid::new_v4());
    auth_service::create_user(pool, Role::Owner, email, &password, Role::Owner)
        .await
        .expect("seed owner");

    let app = router(AppState::new(pool.clone(), false)).layer(CookieManagerLayer::new());
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

#[sqlx::test]
async fn json_extractor_rejections_return_problem_json(pool: sqlx::PgPool) {
    let cookie = login_as_owner(&pool).await;
    let app = router(AppState::new(pool, false)).layer(CookieManagerLayer::new());
    let dummy_id = Uuid::now_v7();

    // 1. Malformed JSON syntax -> 400 Bad Request with problem+json across routes.
    let malformed_routes = [
        ("POST", "/api/v1/auth/login".to_string(), false),
        ("POST", "/api/v1/customers".to_string(), true),
        ("POST", "/api/v1/products".to_string(), true),
        ("POST", "/api/v1/suppliers".to_string(), true),
        ("POST", "/api/v1/wallets".to_string(), true),
        ("POST", format!("/api/v1/sales/{dummy_id}/items"), true),
        ("POST", format!("/api/v1/purchases/{dummy_id}/items"), true),
        (
            "POST",
            format!("/api/v1/sales/{dummy_id}/credit-notes"),
            true,
        ),
        (
            "POST",
            format!("/api/v1/purchases/{dummy_id}/returns"),
            true,
        ),
        (
            "POST",
            format!("/api/v1/customers/{dummy_id}/receipts"),
            true,
        ),
        (
            "POST",
            format!("/api/v1/suppliers/{dummy_id}/payments"),
            true,
        ),
    ];

    for (method, uri, with_cookie) in malformed_routes {
        let mut builder = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::CONTENT_TYPE, "application/json");
        if with_cookie {
            builder = builder.header(header::COOKIE, &cookie);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from("{not valid json")).unwrap())
            .await
            .unwrap();
        assert_problem_json(response, StatusCode::BAD_REQUEST, &format!("syntax {uri}")).await;
    }

    // 2. Valid JSON with invalid field types / missing fields -> 422 problem+json.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/customers")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(r#"{"name": 12345}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(
        response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "wrong field type",
    )
    .await;

    // 3. Missing / wrong Content-Type -> 415 Unsupported Media Type with problem+json.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/customers")
                .header(header::CONTENT_TYPE, "text/plain")
                .header(header::COOKIE, &cookie)
                .body(Body::from(r#"{"name": "Ana"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem_json(
        response,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "missing json content-type",
    )
    .await;
}

#[sqlx::test]
async fn path_and_query_extractor_rejections_return_problem_json(pool: sqlx::PgPool) {
    let cookie = login_as_owner(&pool).await;
    let app = router(AppState::new(pool, false)).layer(CookieManagerLayer::new());
    let dummy_id = Uuid::now_v7();

    // Invalid UUID path parameters across every module -> 400 problem+json.
    let bad_paths = [
        ("GET", "/api/v1/sales/not-a-uuid"),
        ("GET", "/api/v1/customers/not-a-uuid"),
        ("GET", "/api/v1/products/not-a-uuid"),
        ("GET", "/api/v1/suppliers/not-a-uuid"),
        ("GET", "/api/v1/wallets/not-a-uuid"),
        ("GET", "/api/v1/purchases/not-a-uuid"),
        ("GET", "/api/v1/customers/not-a-uuid/account"),
        ("GET", "/api/v1/suppliers/not-a-uuid/account"),
        ("GET", "/api/v1/sales/not-a-uuid/credit-notes"),
        ("GET", "/api/v1/purchases/not-a-uuid/returns"),
        ("PUT", "/api/v1/users/not-a-uuid/password"),
    ];

    for (method, uri) in bad_paths {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, &cookie);
        if method == "PUT" {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let body = if method == "PUT" {
            Body::from(r#"{"new_password":"long-enough-password"}"#)
        } else {
            Body::empty()
        };
        let response = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        assert_problem_json(response, StatusCode::BAD_REQUEST, &format!("path {uri}")).await;
    }

    // Invalid query parameters across every query-taking module -> 400 problem+json.
    let bad_queries = [
        "/api/v1/customers?limit=not-a-number".to_string(),
        "/api/v1/products?limit=not-a-number".to_string(),
        "/api/v1/suppliers?limit=not-a-number".to_string(),
        "/api/v1/purchases?limit=not-a-number".to_string(),
        "/api/v1/wallets?include_inactive=not-a-bool".to_string(),
        format!("/api/v1/customers/{dummy_id}/receipts?limit=not-a-number"),
        format!("/api/v1/suppliers/{dummy_id}/payments?limit=not-a-number"),
    ];

    for uri in bad_queries {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&uri)
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem_json(response, StatusCode::BAD_REQUEST, &format!("query {uri}")).await;
    }
}
