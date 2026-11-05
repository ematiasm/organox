//! Authentication extractors.
//!
//! `AuthUser` is the ONLY way a handler knows the actor's identity. There is
//! no other path: a handler that needs to know who the actor is requests this
//! extractor in its signature, and the compiler requires it to be available.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tower_cookies::Cookies;

use crate::AppState;
use crate::auth::model::Role;
use crate::auth::service::{self, AuthenticatedUser};
use crate::auth::token::{SessionToken, TokenHash};
use crate::error::AppError;

/// Session cookie name.
pub const SESSION_COOKIE: &str = "session";

/// Authenticated user. Fails with 401 if there is no valid session.
///
/// As `FromRequestParts`, it does not consume the body: it can precede
/// `Json<T>` in the handler signature.
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

    /// Authorization: 403 if the role is insufficient. The rule lives in `Role`, not here.
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

        // Extend the session in the background: this request should not wait
        // for an UPDATE that does not affect its result.
        let pool = state.pool.clone();
        tokio::spawn(async move {
            service::touch(&pool, &token).await;
        });

        Ok(AuthUser(user))
    }
}
