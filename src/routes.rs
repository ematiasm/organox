//! Rutas. La raíz de la API vive bajo `/api/v1` desde el día uno: versionar
//! después implica romper clientes, versionar ahora es gratis.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use crate::AppState;
use crate::db;
use crate::error::AppError;

/// Directorio con el build del frontend. Configurable para no atar el binario
/// a una ruta relativa fija.
const FRONTEND_DIR: &str = "frontend/build";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/api/v1", api_v1(state.clone()))
        // El fallback va ÚLTIMO y después de `/api/v1`: si se declara antes, un
        // `fetch` a la API que no matchea ninguna ruta recibe `index.html` en
        // vez de un 404 JSON, y el frontend falla con un error de parseo que no
        // dice nada.
        .fallback(spa_fallback)
        .with_state(state)
}

/// Sirve el build de SvelteKit.
///
/// Como es una SPA (`adapter-static` con `fallback: 'index.html'`), cualquier
/// ruta que no sea un archivo real devuelve `index.html` y el router del
/// navegador la resuelve. Sin esto, `/products` funciona al navegar pero da 404
/// al refrescar.
fn frontend_service() -> ServeDir<ServeFile> {
    ServeDir::new(FRONTEND_DIR).fallback(ServeFile::new(format!("{FRONTEND_DIR}/index.html")))
}

/// Fallback de todas las rutas no reconocidas.
///
/// Dos comportamientos distintos según el prefijo, y esto importa:
///
///  * **`/api/*` sin ruta declarada** → `404` JSON (`application/problem+json`).
///    Si acá se devolviera `index.html`, un `fetch` a un endpoint inexistente
///    recibiría HTML con status 200 y el frontend fallaría con un error de
///    parseo que no dice nada. Lo detectó el test
///    `unknown_api_route_returns_404`.
///  * **Cualquier otra ruta** → el build de la SPA (archivo real, o
///    `index.html` para las rutas que resuelve el router del navegador).
async fn spa_fallback(request: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    use tower::ServiceExt;

    if request.uri().path().starts_with("/api/") {
        return AppError::NotFound.into_response();
    }

    match frontend_service().oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "no se pudo servir el frontend");
            AppError::Internal("no se pudo servir el frontend".to_string()).into_response()
        }
    }
}

/// Acá se enchufan los módulos de dominio a medida que existan.
fn api_v1(state: AppState) -> Router<AppState> {
    Router::new()
        // Swagger UI expone el spec en `/openapi.json`; declararlo aparte
        // provocaba "Overlapping method route". La UI vive en `/api/v1/docs`.
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/openapi.json", crate::openapi::spec()),
        )
        .merge(crate::auth::handlers::router(state.clone()))
        .merge(crate::catalog::handlers::router())
        .merge(crate::sales::handlers::router())
}

/// Liveness: el proceso está vivo. No toca dependencias externas.
/// Si esta ruta falla, el orquestador debe reiniciar el proceso.
async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Readiness: el proceso puede atender tráfico. Si esta falla, el balanceador
/// debe sacarlo de rotación pero NO reiniciarlo.
async fn readyz(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    db::ping(&state.pool)
        .await
        .map_err(|e| AppError::DatabaseUnavailable(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
