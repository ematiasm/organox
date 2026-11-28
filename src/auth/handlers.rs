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
use tower_cookies::Cookies;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::extract::{AuthUser, removal_cookie, session_cookie};
use crate::auth::model::Role;
use crate::auth::service::{self, AuthError, LoginOutcome};
use crate::error::AppError;
use crate::extract::{AppJson, AppPath};

pub fn router(state: AppState) -> Router<AppState> {
    router_with_body_limit(state, crate::routes::AUTH_BODY_LIMIT_BYTES)
}

pub fn router_with_body_limit(state: AppState, body_limit_bytes: usize) -> Router<AppState> {
    use axum::middleware::from_fn_with_state;
    use axum::routing::{get, post, put};
    use tower_http::limit::RequestBodyLimitLayer;

    // Apply the IP limit ONLY to credential-checking routes (`login` and
    // `password`). Applying it to the entire API would penalize a legitimate
    // polling frontend and add nothing against brute force.
    let guarded_auth = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/password", post(change_password))
        .route_layer(from_fn_with_state(
            state.clone(),
            crate::ratelimit::middleware::limit_by_ip,
        ));

    Router::new()
        .merge(guarded_auth)
        .route("/auth/logout", post(logout))
        .route("/auth/me", axum::routing::get(me))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}/password", put(reset_password))
        // Auth payloads carry at most an email and two bounded passwords, so
        // cap them well below the global router limit before JSON buffering.
        .route_layer(RequestBodyLimitLayer::new(body_limit_bytes))
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

#[derive(Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ResetPasswordRequest {
    pub new_password: String,
}

// --- Handlers -----------------------------------------------------------

fn request_user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
}

async fn login(
    State(state): State<AppState>,
    cookies: Cookies,
    headers: HeaderMap,
    AppJson(body): AppJson<LoginRequest>,
) -> Result<Response, AppError> {
    let user_agent = request_user_agent(&headers);

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

async fn list_users(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    // Same authorization rule as user creation, before reading the users.
    user.require_user_management()?;

    let users = service::list_users(&state.pool, user.role()).await?;

    Ok(Json(
        users
            .into_iter()
            .map(|found| UserResponse {
                id: found.id,
                email: found.email,
                role: found.role,
            })
            .collect(),
    ))
}

async fn create_user(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(body): AppJson<CreateUserRequest>,
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

async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    cookies: Cookies,
    headers: HeaderMap,
    AppJson(body): AppJson<ChangePasswordRequest>,
) -> Result<StatusCode, AppError> {
    let session = service::change_password(
        &state.pool,
        &state.account_limiter,
        user.id(),
        user.email(),
        &body.current_password,
        &body.new_password,
        request_user_agent(&headers),
    )
    .await?;

    cookies.add(session_cookie(
        session.token.as_str().to_string(),
        session.expires_at,
        state.cookie_secure,
    ));

    Ok(StatusCode::NO_CONTENT)
}

async fn reset_password(
    State(state): State<AppState>,
    user: AuthUser,
    cookies: Cookies,
    headers: HeaderMap,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<ResetPasswordRequest>,
) -> Result<StatusCode, AppError> {
    // Same owner-only gate as user creation, before any DB work or hashing.
    user.require_user_management()?;

    if let Some(session) = service::reset_password(
        &state.pool,
        user.role(),
        id,
        &body.new_password,
        user.id(),
        request_user_agent(&headers),
    )
    .await?
    {
        cookies.add(session_cookie(
            session.token.as_str().to_string(),
            session.expires_at,
            state.cookie_secure,
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

// --- Errors -------------------------------------------------------------

impl From<AuthError> for AppError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials => AppError::Unauthorized,
            // A missing permission is not a missing session: the client must
            // not be told to log in again when logging in changes nothing.
            AuthError::Forbidden => AppError::Forbidden,
            AuthError::AccountLocked { until } => AppError::TooManyRequests {
                retry_after_secs: (until - chrono::Utc::now()).num_seconds().max(1) as u64,
            },
            AuthError::Validation(v) => AppError::Validation(v.to_string()),
            AuthError::EmailTaken => {
                AppError::Conflict("the email is already registered".to_string())
            }
            AuthError::NotFound => AppError::NotFound,
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

/// List every user, ordered by email. Only `owner`.
#[utoipa::path(
    get,
    path = "/api/v1/users",
    tag = "auth",
    responses(
        (status = 200, description = "Users ordered by email", body = Vec<UserResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 403, description = "Role does not permit managing users", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_users_doc() {}

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

/// Change the current user's password.
///
/// Requires the current password. The new one must satisfy the password rule.
/// Every previous session of the user — including the caller's — is revoked
/// atomically and replaced with a rotated `session` cookie.
///
/// `429` means the account or IP is temporarily throttled after failed
/// attempts; includes `Retry-After` with the remaining seconds.
#[utoipa::path(
    post,
    path = "/api/v1/auth/password",
    tag = "auth",
    request_body = ChangePasswordRequest,
    responses(
        (status = 204, description = "Password changed, previous sessions revoked and caller session rotated",
         headers(("Set-Cookie", description = "session=<new-token>; HttpOnly; SameSite=Strict"))),
        (status = 401, description = "Wrong current password or no session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid new password", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 429, description = "Account locked or IP rate limit exceeded", body = crate::openapi::Problem,
         content_type = "application/problem+json",
         headers(("Retry-After", description = "Seconds until retry"))),
    )
)]
#[allow(dead_code)]
fn change_password_doc() {}

/// Reset another user's password. Only `owner`.
///
/// No current password is requested: the owner cannot know it. Every previous
/// session of the target is revoked atomically; when the owner resets their
/// own password, a rotated `session` cookie is emitted.
#[utoipa::path(
    put,
    path = "/api/v1/users/{id}/password",
    tag = "auth",
    request_body = ResetPasswordRequest,
    params(("id" = Uuid, Path, description = "Target user id")),
    responses(
        (status = 204, description = "Password reset, target sessions revoked (and rotated Set-Cookie emitted on self-reset)"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 403, description = "Role does not permit managing users", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "User not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid new password", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn reset_password_doc() {}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing permission is `403`, not `401`. The service-level `Forbidden`
    /// is defense in depth: the handlers already reject first through
    /// `require_user_management`, so only this conversion decides the code
    /// should that gate ever move.
    #[test]
    fn forbidden_is_403_and_bad_credentials_stay_401() {
        let forbidden = AppError::from(AuthError::Forbidden).into_response();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let unauthorized = AppError::from(AuthError::InvalidCredentials).into_response();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    }
}
