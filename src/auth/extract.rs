//! Extractores de autenticación.
//!
//! `AuthUser` es la ÚNICA forma en que un handler conoce la identidad del
//! actor. No existe otra vía: si un handler necesita saber quién es, pide este
//! extractor por firma y el compilador exige que esté disponible.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tower_cookies::Cookies;

use crate::AppState;
use crate::auth::model::Role;
use crate::auth::service::{self, AuthenticatedUser};
use crate::auth::token::{SessionToken, TokenHash};
use crate::error::AppError;

/// Nombre de la cookie de sesión.
pub const SESSION_COOKIE: &str = "session";

/// Usuario autenticado. Falla con 401 si no hay sesión válida.
///
/// Como `FromRequestParts`, no consume el body: puede ir antes de
/// `Json<T>` en la firma del handler.
pub struct AuthUser(pub AuthenticatedUser);

impl AuthUser {
    pub fn id(&self) -> uuid::Uuid {
        self.0.id
    }

    pub fn email(&self) -> &str {
        &self.0.email
    }

    pub fn role(&self) -> Role {
        self.0.role
    }

    pub fn token_hash(&self) -> &TokenHash {
        &self.0.token_hash
    }

    /// Autorización: 403 si el rol no alcanza. La regla vive en `Role`, no acá.
    pub fn require_user_management(&self) -> Result<(), AppError> {
        if self.0.can_manage_users() {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookies = Cookies::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::Unauthorized)?;

        let cookie = cookies.get(SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
        let token = SessionToken::from_raw(cookie.value());

        let user = service::authenticate(&state.pool, &token)
            .await?
            .ok_or(AppError::Unauthorized)?;

        // La sesión se extiende en segundo plano: no queremos que esta request
        // espere un UPDATE que no afecta su resultado.
        let pool = state.pool.clone();
        tokio::spawn(async move {
            service::touch(&pool, &token).await;
        });

        Ok(AuthUser(user))
    }
}
