//! HTTP skeleton integration tests.
//!
//! These exercise the in-memory `Router` without opening a port, against a
//! real ephemeral database created and destroyed by `#[sqlx::test]` per test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::routes::router;
use organox::{AppState, db};
use tower::ServiceExt;

/// Liveness must not depend on external services: 204 while the process is alive.
#[sqlx::test]
async fn healthz_returns_204_without_touching_db(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// Readiness depends on the database: a healthy pool returns 204.
#[sqlx::test]
async fn readyz_returns_204_when_db_responds(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// Readiness must return 503 if the database is unreachable, not 500 or hang.
#[tokio::test]
async fn readyz_returns_503_when_db_is_unreachable() {
    // Deliberately closed port: the connection fails quickly and the pool has
    // no usable connections.
    let unreachable = "postgres://organox:organox_dev_password@127.0.0.1:1/organox";
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .max_connections(1)
        .connect_lazy(unreachable)
        .expect("connect_lazy does not validate the connection");

    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// An unknown route under /api/v1 must not return 2xx.
#[sqlx::test]
async fn unknown_api_route_returns_404(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The request ID is generated and propagated in the response by `routes::router`.
#[sqlx::test]
async fn request_id_is_propagated(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let generated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let generated_id = generated
        .headers()
        .get("x-request-id")
        .expect("the response must include x-request-id")
        .to_str()
        .expect("x-request-id must be valid ASCII");
    assert!(
        uuid::Uuid::parse_str(generated_id).is_ok(),
        "generated x-request-id must be a UUID, got {generated_id:?}"
    );

    let incoming = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "client-req-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        incoming
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("client-req-123"),
        "caller-provided x-request-id must be propagated"
    );
}

/// Readiness uses `ping`: it must succeed against a live database.
#[sqlx::test]
async fn ping_succeeds_against_live_db(pool: sqlx::PgPool) {
    db::ping(&pool).await.expect("ping must succeed");
}

/// Error contract: RFC 7807 requires `application/problem+json`.
///
/// The backend was observed returning `application/json` and the header was
/// added manually; this test prevents it from diverging from the spec again.
#[sqlx::test]
async fn errors_use_problem_json_content_type(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let header = response
        .headers()
        .get("content-type")
        .expect("every error must declare content-type")
        .to_str()
        .unwrap();

    assert_eq!(
        header, "application/problem+json",
        "errors must use the RFC 7807 Content-Type"
    );
}

/// The SPA is served on non-API routes, while the API still returns 404 JSON.
///
/// This test pins the distinction that broke the frontend: a `fetch` to an
/// unknown endpoint must NOT receive `index.html` with status 200.
#[sqlx::test]
async fn spa_shell_served_outside_api(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    // SPA route: returns the shell (or 404 if it has not been built).
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
        "the SPA route must not return a server error: {}",
        spa.status()
    );

    // Unknown API route: 404 problem+json, never HTML.
    let api = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/no-existe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        api.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
}

/// Exact `/api` (without trailing slash) must return RFC 7807 JSON 404,
/// never falling through to the SPA fallback.
#[tokio::test]
async fn exact_api_path_returns_problem_json_404() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/never_connected")
        .expect("lazy pool");
    let app = router(AppState::new(pool, false));

    let response = app
        .oneshot(Request::builder().uri("/api").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "exact /api must return RFC 7807 problem+json 404"
    );
}

/// `AppState::with_frontend_dir` serves static assets and SPA fallback from the
/// configured directory while guarding `/api` and `/api/*` (and leaving sibling
/// paths like `/api-docs` to the SPA).
#[tokio::test]
async fn spa_uses_configured_frontend_dir_and_guards_api_prefix() {
    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let dir =
        TempDir(std::env::temp_dir().join(format!("organox-spa-test-{}", uuid::Uuid::now_v7())));
    std::fs::create_dir_all(&dir.0).expect("create temp frontend dir");
    std::fs::write(dir.0.join("index.html"), "<html>spa-custom-root</html>")
        .expect("write index.html");
    std::fs::write(dir.0.join("hello.txt"), "static-asset-ok").expect("write asset");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/never_connected")
        .expect("lazy pool");
    let app = router(AppState::new(pool, false).with_frontend_dir(&dir.0));

    // Real file in the custom directory.
    let asset = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/hello.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    let body = axum::body::to_bytes(asset.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"static-asset-ok");

    // Client-side SPA route and non-`/api` sibling path fall back to index.html.
    for spa_path in ["/products", "/api-docs"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(spa_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "expected SPA 200 for {spa_path}"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"<html>spa-custom-root</html>");
    }

    // Exact `/api` and `/api/*` return RFC 7807 JSON 404 even when index.html exists.
    for api_path in ["/api", "/api/", "/api/unknown"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(api_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for {api_path}"
        );
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "expected problem+json for {api_path}"
        );
    }
}
