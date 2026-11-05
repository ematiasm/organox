//! Application logic. The binary (`main.rs`) only assembles and starts it.

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

/// State shared by all handlers.
///
/// Cheap `Clone`: `PgPool` internally uses an `Arc`. Dependencies are added
/// here as they arise.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Per-account failed-attempt limit. Lives in Postgres.
    pub account_limiter: AccountLimiter,
    /// Per-IP request limit. Lives in memory, shared between requests through
    /// the `Arc`.
    pub ip_limiter: Arc<crate::ratelimit::IpLimiter>,
    /// Session cookies use `Secure` except on local HTTP. In production this
    /// must always be `true`; the dev default is `false` so `curl` against
    /// http://localhost works.
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
