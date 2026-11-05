//! Routes. The API root lives under `/api/v1` from day one: versioning later
//! breaks clients; versioning now is free.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use crate::AppState;
use crate::db;
use crate::error::AppError;

/// Frontend build directory. Configurable to avoid tying the binary to a fixed
/// relative path.
const FRONTEND_DIR: &str = "frontend/build";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/api/v1", api_v1(state.clone()))
        // The fallback goes LAST and after `/api/v1`: if declared earlier, an
        // API `fetch` matching no route receives `index.html` instead of a JSON
        // 404, and the frontend fails with an uninformative parsing error.
        .fallback(spa_fallback)
        .with_state(state)
}

/// Serve the SvelteKit build.
///
/// As this is an SPA (`adapter-static` with `fallback: 'index.html'`), any
/// route that is not a real file returns `index.html` and the browser router
/// resolves it. Without this, `/products` works when navigating but returns 404
/// on refresh.
fn frontend_service() -> ServeDir<ServeFile> {
    ServeDir::new(FRONTEND_DIR).fallback(ServeFile::new(format!("{FRONTEND_DIR}/index.html")))
}

/// Fallback for all unrecognized routes.
///
/// Two different behaviors depending on the prefix, and this matters:
///
///  * **`/api/*` without a declared route** → JSON `404` (`application/problem+json`).
///    Returning `index.html` here would give a `fetch` to a nonexistent endpoint
///    HTML with status 200, and the frontend would fail with an uninformative
///    parsing error. The `unknown_api_route_returns_404` test caught this.
///  * **Any other route** → the SPA build (a real file, or `index.html` for
///    routes resolved by the browser router).
async fn spa_fallback(request: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    use tower::ServiceExt;

    if request.uri().path().starts_with("/api/") {
        return AppError::NotFound.into_response();
    }

    match frontend_service().oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "could not serve the frontend");
            AppError::Internal("could not serve the frontend".to_string()).into_response()
        }
    }
}

/// Domain modules are plugged in here as they become available.
fn api_v1(state: AppState) -> Router<AppState> {
    Router::new()
        // Swagger UI exposes the spec at `/openapi.json`; declaring it separately
        // caused "Overlapping method route". The UI lives at `/api/v1/docs`.
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/openapi.json", crate::openapi::spec()),
        )
        .merge(crate::auth::handlers::router(state.clone()))
        .merge(crate::catalog::handlers::router())
        .merge(crate::sales::handlers::router())
}

/// Liveness: the process is alive. Does not touch external dependencies.
/// If this route fails, the orchestrator should restart the process.
async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Readiness: the process can handle traffic. If this fails, the load balancer
/// should remove it from rotation but NOT restart it.
async fn readyz(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    db::ping(&state.pool)
        .await
        .map_err(|e| AppError::DatabaseUnavailable(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
