//! Handlers HTTP de autenticación.
//!
//! Responsabilidad acotada: traducir HTTP ↔ casos de uso. Sin SQL, sin reglas
//! de negocio, sin decisión de autorización (esa vive en el service y en los
//! extractores).

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

    // El límite por IP se aplica SOLO al login. Aplicarlo a toda la API
    // castigaría a un frontend legítimo que hace polling y no aporta nada
    // contra fuerza bruta.
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
// Nunca se serializa la struct del dominio. Estos tipos son el contrato HTTP.

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
            // Se calcula el resto en segundos y se informa por `Retry-After`.
            // Sin esto el cliente no sabe si esperar 2 segundos o 2 días.
            let secs = (until - chrono::Utc::now()).num_seconds().max(1) as u64;
            tracing::warn!(
                email = %body.email,
                retry_after_secs = secs,
                "login bloqueado por intentos fallidos"
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
    // Se borra la sesión en la DB, no solo la cookie: si la cookie fue robada,
    // el token deja de servir en el próximo request.
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
    // 403 antes de tocar la DB o gastar un hash de argon2.
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
    // Strict: la cookie no viaja en requests originadas por otro sitio, lo que
    // corta CSRF de raíz para esta API. Lax también serviría con un frontend
    // en otro origen, pero entonces haría falta token CSRF explícito.
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
    // Expirar en el pasado es la forma canónica de borrar una cookie.
    cookie.make_removal();
    cookie
}

// --- Errores ------------------------------------------------------------

impl From<AuthError> for AppError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials | AuthError::Forbidden => AppError::Unauthorized,
            AuthError::AccountLocked { until } => AppError::TooManyRequests {
                retry_after_secs: (until - chrono::Utc::now()).num_seconds().max(1) as u64,
            },
            AuthError::Validation(v) => AppError::Validation(v.to_string()),
            AuthError::EmailTaken => AppError::Conflict("el email ya está registrado".to_string()),
            AuthError::Internal(repo_err) => AppError::Internal(repo_err.to_string()),
        }
    }
}

// --- Documentación OpenAPI ---------------------------------------------
//
// Stubs: solo existen para que `#[utoipa::path]` genere el PathItem. La ruta
// real la sirve el handler correspondiente arriba. Se declaran acá, junto al
// handler, para que un cambio de firma rompa la compilación en el mismo lugar.

/// Inicia sesión.
///
/// Devuelve la cookie `session` (HttpOnly, SameSite=Strict) y el usuario.
///
/// `429` significa cuenta bloqueada por intentos fallidos; incluye
/// `Retry-After` con los segundos que faltan.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Sesión iniciada", body = LoginResponse,
         headers(("Set-Cookie", description = "session=<token>; HttpOnly; SameSite=Strict"))),
        (status = 401, description = "Credenciales inválidas", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 429, description = "Cuenta bloqueada por intentos fallidos", body = crate::openapi::Problem,
         content_type = "application/problem+json",
         headers(("Retry-After", description = "Segundos hasta poder reintentar"))),
    )
)]
#[allow(dead_code)]
fn login_doc() {}

/// Cierra la sesión en el servidor, no solo en el cliente.
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "Sesión borrada"),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn logout_doc() {}

/// Usuario autenticado en la request actual.
#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "Usuario actual", body = UserResponse),
        (status = 401, description = "Sin sesión o sesión vencida", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn me_doc() {}

/// Crea un usuario. Solo `owner`.
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "auth",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "Usuario creado", body = CreateUserResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 403, description = "El rol no permite gestionar usuarios", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "El email ya está registrado", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Entrada inválida (email o contraseña)", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_user_doc() {}
