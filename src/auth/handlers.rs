//! Authentication HTTP handlers.
//!
//! Limited responsibility: translate HTTP ↔ use cases. No SQL, business rules,
//! or authorization decisions (those live in the service and extractors).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tower_cookies::cookie::{Cookie, SameSite};
use tower_cookies::{Cookies, cookie};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::extract::{AuthUser, SESSION_COOKIE};
use crate::auth::model::Role;
use crate::auth::service::{self, AuthError, LoginOutcome};
use crate::error::AppError;

pub fn router(state: AppState) -> Router<AppState> {
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;

    // Apply the IP limit ONLY to login. Applying it to the entire API would
    // penalize a legitimate polling frontend and add nothing against brute force.
    let guarded_login =
        Router::new()
            .route("/auth/login", post(login))
            .route_layer(from_fn_with_state(
                state.clone(),
                crate::ratelimit::middleware::limit_by_ip,
            ));

    Router::new()
        .merge(guarded_login)
        .route("/auth/logout", post(logout))
        .route("/auth/me", axum::routing::get(me))
        .route("/users", post(create_user))
}

// --- DTOs ---------------------------------------------------------------
//
// Never serialize the domain struct. These types are the HTTP contract.

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize, ToSchema)]
pub struct UserResponse {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
}

#[derive(Serialize, ToSchema)]
pub struct LoginResponse {
    pub user: UserResponse,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    pub role: Role,
}

#[derive(Serialize, ToSchema)]
pub struct CreateUserResponse {
    pub id: Uuid,
}

// --- Handlers -----------------------------------------------------------

async fn login(
    State(state): State<AppState>,
    cookies: Cookies,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AppError> {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());

    match service::login(
        &state.pool,
        &state.account_limiter,
        &body.email,
        &body.password,
        user_agent,
    )
    .await?
    {
        LoginOutcome::InvalidCredentials => Err(AppError::Unauthorized),
        LoginOutcome::Locked { until } => {
            // Compute the remaining seconds and report them via `Retry-After`.
            // Without this the client cannot know whether to wait 2 seconds or 2 days.
            let secs = (until - chrono::Utc::now()).num_seconds().max(1) as u64;
            tracing::warn!(
                email = %body.email,
                retry_after_secs = secs,
                "login locked due to failed attempts"
            );
            Err(AppError::TooManyRequests {
                retry_after_secs: secs,
            })
        }
        LoginOutcome::Authenticated { user, session } => {
            cookies.add(session_cookie(
                session.token.as_str().to_string(),
                session.expires_at,
                state.cookie_secure,
            ));

            Ok(Json(LoginResponse {
                user: UserResponse {
                    id: user.id,
                    email: user.email,
                    role: user.role,
                },
                expires_at: session.expires_at,
            })
            .into_response())
        }
    }
}

async fn logout(
    State(state): State<AppState>,
    user: AuthUser,
    cookies: Cookies,
) -> Result<StatusCode, AppError> {
    // Delete the session from the DB, not just the cookie: if the cookie was
    // stolen, the token stops working on the next request.
    service::logout_by_hash(&state.pool, user.token_hash()).await?;
    cookies.remove(removal_cookie(state.cookie_secure));

    Ok(StatusCode::NO_CONTENT)
}

async fn me(user: AuthUser) -> Json<UserResponse> {
    Json(UserResponse {
        id: user.id(),
        email: user.email().to_string(),
        role: user.role(),
    })
}

async fn create_user(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreateUserResponse>), AppError> {
    // 403 before touching the DB or spending an argon2 hash.
    user.require_user_management()?;

    let id = service::create_user(
        &state.pool,
        user.role(),
        &body.email,
        &body.password,
        body.role,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(CreateUserResponse { id })))
}

// --- Cookies ------------------------------------------------------------

fn session_cookie(value: String, expires_at: DateTime<Utc>, secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, value);
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    // Strict: the cookie does not travel on requests originating from another
    // site, stopping CSRF at the root for this API. Lax would also support a
    // frontend on another origin, but would require an explicit CSRF token.
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path("/");
    cookie.set_expires(
        cookie::time::OffsetDateTime::from_unix_timestamp(expires_at.timestamp())
            .unwrap_or_else(|_| cookie::time::OffsetDateTime::now_utc()),
    );
    cookie
}

fn removal_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, "");
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path("/");
    // Expiring in the past is the canonical way to delete a cookie.
    cookie.make_removal();
    cookie
}

// --- Errors -------------------------------------------------------------

impl From<AuthError> for AppError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials | AuthError::Forbidden => AppError::Unauthorized,
            AuthError::AccountLocked { until } => AppError::TooManyRequests {
                retry_after_secs: (until - chrono::Utc::now()).num_seconds().max(1) as u64,
            },
            AuthError::Validation(v) => AppError::Validation(v.to_string()),
            AuthError::EmailTaken => {
                AppError::Conflict("the email is already registered".to_string())
            }
            AuthError::Internal(repo_err) => AppError::Internal(repo_err.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------
//
// Stubs exist only for `#[utoipa::path]` to generate the PathItem. The real
// route is served by the corresponding handler above. They are declared here,
// alongside the handler, so a signature change breaks compilation in one place.

/// Log in.
///
/// Returns the `session` cookie (HttpOnly, SameSite=Strict) and the user.
///
/// `429` means the account is locked after failed attempts; includes
/// `Retry-After` with the remaining seconds.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Session started", body = LoginResponse,
         headers(("Set-Cookie", description = "session=<token>; HttpOnly; SameSite=Strict"))),
        (status = 401, description = "Invalid credentials", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 429, description = "Account locked after failed attempts", body = crate::openapi::Problem,
         content_type = "application/problem+json",
         headers(("Retry-After", description = "Seconds until retry"))),
    )
)]
#[allow(dead_code)]
fn login_doc() {}

/// End the session on the server, not just the client.
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "Session deleted"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn logout_doc() {}

/// Authenticated user in the current request.
#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "Current user", body = UserResponse),
        (status = 401, description = "No session or expired session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn me_doc() {}

/// Create a user. Only `owner`.
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "auth",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "User created", body = CreateUserResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 403, description = "Role does not permit managing users", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Email is already registered", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input (email or password)", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_user_doc() {}
