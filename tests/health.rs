//! Tests de integración del esqueleto HTTP.
//!
//! Se ejecutan contra el `Router` en memoria (sin abrir puerto) y contra una
//! base de datos real y efímera que `#[sqlx::test]` crea y destruye por test.

use organox::routes::router;
use organox::{AppState, db};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Liveness no debe depender de nada externo: 204 siempre que el proceso viva.
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

/// Readiness sí depende de la DB: con el pool sano responde 204.
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

/// Readiness debe fallar con 503 si la DB no responde, no con 500 ni colgándose.
#[tokio::test]
async fn readyz_returns_503_when_db_is_unreachable() {
    // Puerto cerrado a propósito: la conexión falla rápido y el pool queda
    // sin conexiones utilizables.
    let unreachable = "postgres://organox:organox_dev_password@127.0.0.1:1/organox";
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .max_connections(1)
        .connect_lazy(unreachable)
        .expect("connect_lazy no valida la conexión");

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

/// Una ruta inexistente bajo /api/v1 no debe responder 2xx.
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

/// El request id se genera y se propaga en la respuesta.
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
        "la respuesta debe traer x-request-id"
    );
}

/// `ping` es lo que usa readiness: debe funcionar contra una DB viva.
#[sqlx::test]
async fn ping_succeeds_against_live_db(pool: sqlx::PgPool) {
    db::ping(&pool).await.expect("el ping debe funcionar");
}

/// Contrato de errores: RFC 7807 exige `application/problem+json`.
///
/// Se verificó empíricamente que el backend respondía `application/json` y el
/// header se agregó a mano; este test evita que vuelva a divergir del spec.
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
        .expect("todo error debe declarar content-type")
        .to_str()
        .unwrap();

    assert_eq!(
        header, "application/problem+json",
        "los errores deben usar el Content-Type de RFC 7807"
    );
}

/// La SPA se sirve en rutas no-API, y la API sigue devolviendo 404 JSON.
///
/// Este test fija la distinción que rompía el frontend: un `fetch` a un endpoint
/// inexistente NO debe recibir `index.html` con status 200.
#[sqlx::test]
async fn spa_shell_served_outside_api(pool: sqlx::PgPool) {
    let app = router(AppState::new(pool, false));

    // Ruta de la SPA: devuelve el cascarón (o 404 si el build no está hecho).
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
        "la ruta de SPA no debe devolver error del servidor: {}",
        spa.status()
    );

    // Ruta de API inexistente: 404 problem+json, nunca HTML.
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
