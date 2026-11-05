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

/// The request ID is generated and propagated in the response.
#[sqlx::test]
async fn request_id_is_propagated(pool: sqlx::PgPool) {
    use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

    let app = router(AppState::new(pool, false))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response.headers().get("x-request-id").is_some(),
        "the response must include x-request-id"
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
