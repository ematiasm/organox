//! Lógica de la aplicación. El binario (`main.rs`) solo arma y arranca.

pub mod auth;
pub mod catalog;
pub mod config;
pub mod db;
pub mod error;
pub mod openapi;
pub mod ratelimit;
pub mod routes;
pub mod sales;
pub mod telemetry;

use sqlx::PgPool;
use std::sync::Arc;

use crate::ratelimit::AccountLimiter;

/// Estado compartido por todos los handlers.
///
/// `Clone` barato: `PgPool` es internamente un `Arc`. A medida que aparezcan
/// dependencias se suman acá.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Límite de intentos fallidos por cuenta. Vive en Postgres.
    pub account_limiter: AccountLimiter,
    /// Límite de requests por IP. Vive en memoria, compartido entre requests
    /// por el `Arc`.
    pub ip_limiter: Arc<crate::ratelimit::IpLimiter>,
    /// Las cookies de sesión llevan `Secure` salvo en HTTP local. En producción
    /// esto tiene que ser `true` siempre; el default de dev es `false` para que
    /// `curl` sobre http://localhost funcione.
    pub cookie_secure: bool,
}

impl AppState {
    pub fn new(pool: PgPool, cookie_secure: bool) -> Self {
        Self::with_policies(
            pool,
            cookie_secure,
            crate::ratelimit::AccountPolicy::default(),
            crate::ratelimit::IpPolicy::default(),
        )
    }

    pub fn with_policies(
        pool: PgPool,
        cookie_secure: bool,
        account_policy: crate::ratelimit::AccountPolicy,
        ip_policy: crate::ratelimit::IpPolicy,
    ) -> Self {
        Self {
            account_limiter: AccountLimiter::new(pool.clone(), account_policy),
            ip_limiter: Arc::new(crate::ratelimit::IpLimiter::new(ip_policy)),
            pool,
            cookie_secure,
        }
    }
}
