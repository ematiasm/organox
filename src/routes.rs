//! Routes. The API root lives under `/api/v1` from day one: versioning later
//! breaks clients; versioning now is free.

use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_cookies::CookieManagerLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::AppState;
use crate::db;
use crate::error::AppError;

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
    let cookie_secure = state.cookie_secure;
    let frontend = frontend_service(&state.frontend_dir);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/api/v1", api_v1(state.clone(), limits))
        // The fallback goes LAST and after `/api/v1`: if declared earlier, an
        // API `fetch` matching no route receives `index.html` instead of a JSON
        // 404, and the frontend fails with an uninformative parsing error.
        .fallback(move |request| spa_fallback(request, frontend.clone()))
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
        .layer(map_response(move |response| {
            add_security_headers(response, cookie_secure)
        }))
        // CookieManagerLayer must wrap the router: the `Cookies` and `AuthUser`
        // extractors depend on it.
        .layer(CookieManagerLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        // Generate the request id BEFORE the trace span so the span sees it,
        // and propagate it in the response.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

/// Attach baseline HTTP security headers to every response, including errors
/// and middleware rejections.
///
/// `Strict-Transport-Security` is emitted only when `cookie_secure` is `true`
/// (the deployment is served over HTTPS). `includeSubDomains` is omitted so
/// sibling internal services on other subdomains are not forced onto HTTPS.
async fn add_security_headers(mut response: Response, cookie_secure: bool) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if cookie_secure {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
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

/// Serve the SvelteKit build from `dir`.
///
/// As this is an SPA (`adapter-static` with `fallback: 'index.html'`), any
/// route that is not a real file returns `index.html` and the browser router
/// resolves it. Without this, `/products` works when navigating but returns 404
/// on refresh.
///
/// Built once when assembling the router and cloned per fallback request.
fn frontend_service(dir: &Path) -> ServeDir<ServeFile> {
    ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")))
}

/// Fallback for all unrecognized routes.
///
/// Two different behaviors depending on the prefix, and this matters:
///
///  * **`/api` or `/api/*` without a declared route** → JSON `404` (`application/problem+json`).
///    Returning `index.html` here would give a `fetch` to a nonexistent endpoint
///    HTML with status 200, and the frontend would fail with an uninformative
///    parsing error. The `unknown_api_route_returns_404` test caught this.
///  * **Any other route** → the SPA build (a real file, or `index.html` for
///    routes resolved by the browser router).
async fn spa_fallback(
    request: axum::extract::Request,
    frontend: ServeDir<ServeFile>,
) -> axum::response::Response {
    use tower::ServiceExt;

    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        return AppError::NotFound.into_response();
    }

    match frontend.oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "could not serve the frontend");
            AppError::Internal("could not serve the frontend".to_string()).into_response()
        }
    }
}

/// Domain modules are plugged in here as they become available.
fn api_v1(state: AppState, limits: HttpLimits) -> Router<AppState> {
    let mut router = Router::new();

    if state.enable_docs {
        // Swagger UI exposes the spec at `/openapi.json`; declaring it separately
        // caused "Overlapping method route". The UI lives at `/api/v1/docs`.
        router = router.merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/openapi.json", crate::openapi::spec()),
        );
    }

    router
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
