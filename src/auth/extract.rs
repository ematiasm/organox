//! Authentication extractors.
//!
//! `AuthUser` is the ONLY way a handler knows the actor's identity. There is
//! no other path: a handler that needs to know who the actor is requests this
//! extractor in its signature, and the compiler requires it to be available.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use tower_cookies::cookie::{Cookie, SameSite};
use tower_cookies::{Cookies, cookie};

use crate::AppState;
use crate::auth::model::Role;
use crate::auth::service::{self, AuthenticatedUser};
use crate::auth::token::{SessionToken, TokenHash};
use crate::error::AppError;

/// Session cookie name.
pub const SESSION_COOKIE: &str = "session";

/// Build the `session` cookie with uniform security attributes (`HttpOnly`,
/// `SameSite=Strict`, `Path=/`, `Secure` from config, and the exact DB expiry).
pub(crate) fn session_cookie(
    value: String,
    expires_at: DateTime<Utc>,
    secure: bool,
) -> Cookie<'static> {
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

/// Build the removal cookie used on logout.
pub(crate) fn removal_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, "");
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path("/");
    // Expiring in the past is the canonical way to delete a cookie.
    cookie.make_removal();
    cookie
}

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

        let session = service::authenticate_session(&state.pool, &token)
            .await?
            .ok_or(AppError::Unauthorized)?;

        if let Some(expires_at) = session.refreshed_expires_at {
            cookies.add(session_cookie(
                token.as_str().to_string(),
                expires_at,
                state.cookie_secure,
            ));
        }

        Ok(AuthUser(session.user))
    }
}
