//! HTTP security headers and OpenAPI/Swagger UI gating integration tests (#111).

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use organox::AppState;
use organox::routes::{HttpLimits, router, router_with_limits};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

fn assert_base_security_headers<B>(response: &Response<B>, context: &str) {
    let headers = response.headers();
    assert_eq!(
        headers
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("nosniff"),
        "{context}: missing or invalid X-Content-Type-Options"
    );
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("DENY"),
        "{context}: missing or invalid X-Frame-Options"
    );
    assert_eq!(
        headers
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok()),
        Some("frame-ancestors 'none'"),
        "{context}: missing or invalid Content-Security-Policy"
    );
    assert_eq!(
        headers
            .get(header::REFERRER_POLICY)
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer"),
        "{context}: missing or invalid Referrer-Policy"
    );
}

#[sqlx::test]
async fn docs_and_openapi_are_disabled_by_default(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    for path in ["/api/v1/docs", "/api/v1/docs/", "/api/v1/openapi.json"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must return 404 when docs are disabled"
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "{path} 404 must use RFC 7807 problem+json"
        );
        assert_base_security_headers(&response, path);
    }
}

#[sqlx::test]
async fn docs_and_openapi_are_served_when_enabled(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false).with_docs(true));

    let spec = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spec.status(), StatusCode::OK);
    assert_base_security_headers(&spec, "/api/v1/openapi.json");
    let body = axum::body::to_bytes(spec.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid OpenAPI JSON");
    assert!(
        json.get("openapi").and_then(|v| v.as_str()).is_some(),
        "OpenAPI spec must declare an openapi version"
    );

    let docs_redirect = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        docs_redirect.status().is_redirection() || docs_redirect.status() == StatusCode::OK,
        "/api/v1/docs must serve or redirect to Swagger UI, got {}",
        docs_redirect.status()
    );
    assert_base_security_headers(&docs_redirect, "/api/v1/docs");

    let docs_ui = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/docs/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(docs_ui.status(), StatusCode::OK);
    assert_base_security_headers(&docs_ui, "/api/v1/docs/");
}

#[sqlx::test]
async fn security_headers_are_set_across_success_error_spa_and_middleware_responses(
    pool: sqlx::PgPool,
) {
    let limits = HttpLimits {
        request_timeout: Duration::from_secs(30),
        body_limit_bytes: 32,
        auth_body_limit_bytes: 32,
    };
    let app =
        router_with_limits(AppState::new(pool, false), limits).layer(CookieManagerLayer::new());

    // 1. Health endpoint (204)
    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::NO_CONTENT);
    assert_base_security_headers(&health, "/healthz");
    assert!(
        health
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_none(),
        "HSTS must not be emitted when cookie_secure is false"
    );

    // 2. Unauthorized API error (401)
    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    assert_base_security_headers(&unauth, "/api/v1/auth/me");
    assert!(
        unauth
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_none()
    );

    // 3. Unknown API route (404)
    let not_found = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
    assert_base_security_headers(&not_found, "/api/v1/does-not-exist");

    // 4. SPA fallback route
    let spa = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/products")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        spa.status() == StatusCode::OK || spa.status() == StatusCode::NOT_FOUND,
        "SPA route must not return a server error, got {}",
        spa.status()
    );
    assert_base_security_headers(&spa, "/products");

    // 5. Middleware-generated 413 Payload Too Large (Content-Length short-circuit)
    let too_large_cl = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/does-not-exist")
                .header(header::CONTENT_LENGTH, 64)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(too_large_cl.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_base_security_headers(&too_large_cl, "413 content-length short-circuit");

    // 6. Streamed oversized payload on an auth route (413)
    let too_large_stream = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(vec![b'a'; 64]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(too_large_stream.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_base_security_headers(&too_large_stream, "413 streamed payload too large");
}

#[sqlx::test]
async fn hsts_is_emitted_only_when_cookie_secure_is_true(pool: sqlx::PgPool) {
    let https_app = router(AppState::new(pool.clone(), true));
    let https_response = https_app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(https_response.status(), StatusCode::NO_CONTENT);
    assert_base_security_headers(&https_response, "https /healthz");
    assert_eq!(
        https_response
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=31536000"),
        "HSTS must be emitted when cookie_secure is true"
    );

    let http_app = router(AppState::new(pool, false));
    let http_response = http_app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(http_response.status(), StatusCode::NO_CONTENT);
    assert!(
        http_response
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_none(),
        "HSTS must be omitted when cookie_secure is false"
    );
}

#[tokio::test]
async fn security_headers_cover_docs_assets_400_429_503_and_https_errors() {
    use organox::ratelimit::{AccountPolicy, IpPolicy};

    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .max_connections(1)
        .connect_lazy("postgres://organox:organox_dev_password@127.0.0.1:1/organox")
        .expect("connect_lazy does not validate the connection");

    let state = AppState::with_policies(
        pool.clone(),
        true,
        AccountPolicy::default(),
        IpPolicy {
            max_requests: 1,
            window: Duration::from_secs(60),
            burst: 0,
        },
    );
    let app = router_with_limits(
        state,
        HttpLimits {
            request_timeout: Duration::from_secs(5),
            body_limit_bytes: 32,
            auth_body_limit_bytes: 32,
        },
    )
    .layer(CookieManagerLayer::new());

    // Disabled Swagger UI subpaths return 404 problem+json with security headers + HSTS.
    for path in ["/api/v1/docs/index.html", "/api/v1/docs/swagger-ui.css"] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
        assert_base_security_headers(&res, path);
        assert_eq!(
            res.headers()
                .get(header::STRICT_TRANSPORT_SECURITY)
                .and_then(|v| v.to_str().ok()),
            Some("max-age=31536000")
        );
    }

    // 400 malformed JSON, 429 IP rate limit, 503 unreachable DB, and 413 all include headers + HSTS.
    let bad_json = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_json.status(), StatusCode::BAD_REQUEST);
    assert_base_security_headers(&bad_json, "400 malformed json");
    assert_eq!(
        bad_json
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=31536000")
    );

    let rate_limited = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_base_security_headers(&rate_limited, "429 rate limit");
    assert_eq!(
        rate_limited
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=31536000")
    );

    let readyz = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(readyz.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_base_security_headers(&readyz, "503 readyz");
    assert_eq!(
        readyz
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=31536000")
    );

    let too_large = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/does-not-exist")
                .header(header::CONTENT_LENGTH, 64)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_base_security_headers(&too_large, "413 https");
    assert_eq!(
        too_large
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=31536000")
    );

    // Enabled Swagger UI asset serves 200 with security headers.
    let docs_app = router(AppState::new(pool, false).with_docs(true));
    let css = docs_app
        .oneshot(
            Request::builder()
                .uri("/api/v1/docs/swagger-ui.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(css.status(), StatusCode::OK);
    assert_base_security_headers(&css, "/api/v1/docs/swagger-ui.css");
}
