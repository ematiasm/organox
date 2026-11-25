//! Routes. The API root lives under `/api/v1` from day one: versioning later
//! breaks clients; versioning now is free.

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::timeout::TimeoutLayer;

use crate::AppState;
use crate::db;
use crate::error::AppError;

/// Frontend build directory. Configurable to avoid tying the binary to a fixed
/// relative path.
const FRONTEND_DIR: &str = "frontend/build";

/// Default per-request timeout. Well above normal handler latency and the 5 s
/// database acquire timeout, while preventing stalled requests from pinning
/// pool connections and worker tasks indefinitely.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default global request body size cap (1 MiB) applied to every route.
pub const DEFAULT_BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// Tighter request body size cap (16 KiB) for authentication and user
/// management routes, whose valid payloads carry only short credentials.
pub const AUTH_BODY_LIMIT_BYTES: usize = 16 * 1024;

/// Server-side HTTP timeout and request body size limits enforced by the
/// shared router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    pub request_timeout: Duration,
    pub body_limit_bytes: usize,
    pub auth_body_limit_bytes: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
            auth_body_limit_bytes: AUTH_BODY_LIMIT_BYTES,
        }
    }
}

pub fn router(state: AppState) -> Router {
    router_with_limits(state, HttpLimits::default())
}

pub fn router_with_limits(state: AppState, limits: HttpLimits) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/api/v1", api_v1(state.clone(), limits))
        // The fallback goes LAST and after `/api/v1`: if declared earlier, an
        // API `fetch` matching no route receives `index.html` instead of a JSON
        // 404, and the frontend fails with an uninformative parsing error.
        .fallback(spa_fallback)
        .with_state(state)
        // Disable Axum's implicit 2 MiB extractor limit so `RequestBodyLimitLayer`
        // is the single source of truth across JSON and non-JSON routes.
        .layer(DefaultBodyLimit::disable())
        // Use 504 Gateway Timeout rather than 408 Request Timeout: browsers may
        // auto-retry 408 on non-idempotent POST mutations, and 5xx logs as an
        // error in `AppError` to surface pool/DB stalls.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            limits.request_timeout,
        ))
        .layer(RequestBodyLimitLayer::new(limits.body_limit_bytes))
        .layer(map_response(map_middleware_errors))
}

/// Convert plain-text/empty 413 and 504 rejections produced by `tower-http` or
/// Axum body extractors into uniform RFC 7807 `application/problem+json`
/// responses.
async fn map_middleware_errors(response: Response) -> Response {
    let is_problem_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/problem+json"));

    if is_problem_json {
        return response;
    }

    match response.status() {
        StatusCode::GATEWAY_TIMEOUT => AppError::GatewayTimeout.into_response(),
        StatusCode::PAYLOAD_TOO_LARGE => AppError::PayloadTooLarge.into_response(),
        _ => response,
    }
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
fn api_v1(state: AppState, limits: HttpLimits) -> Router<AppState> {
    Router::new()
        // Swagger UI exposes the spec at `/openapi.json`; declaring it separately
        // caused "Overlapping method route". The UI lives at `/api/v1/docs`.
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/openapi.json", crate::openapi::spec()),
        )
        .merge(crate::auth::handlers::router_with_body_limit(
            state.clone(),
            limits.auth_body_limit_bytes,
        ))
        .merge(crate::catalog::handlers::router())
        .merge(crate::customers::handlers::router())
        .merge(crate::customer_accounts::handlers::router())
        .merge(crate::payments::handlers::router())
        .merge(crate::credit_notes::handlers::router())
        .merge(crate::sales::handlers::router())
        .merge(crate::suppliers::handlers::router())
        .merge(crate::supplier_accounts::handlers::router())
        .merge(crate::purchase_returns::handlers::router())
        .merge(crate::purchases::handlers::router())
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
