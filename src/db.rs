//! Postgres connection and the only query in this slice (DB liveness).

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, postgres::PgConnectOptions};
use std::time::Duration;

/// Pool size. Deliberately starts small: sqlx's defaults (10) run out quickly
/// and the error appears only under load. Adjust based on measurement.
const POOL_MAX_CONNECTIONS: u32 = 5;

/// Maximum time to obtain a connection from the pool. Without this, an
/// exhausted pool hangs the request forever instead of failing.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let options: PgConnectOptions = database_url.parse()?;

    PgPoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
}

/// Readiness: the DB responds. Used by `/readyz`.
///
/// `SELECT 1` does not prove migrations have been applied; it proves the
/// pool can talk to Postgres. That is what a readiness probe needs.
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
