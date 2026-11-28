//! Postgres connection, the only query in this slice (DB liveness), and the
//! shared `sqlx` error introspection the repositories use to name constraints.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, postgres::PgConnectOptions};
use std::time::Duration;

/// Default pool size. Deliberately starts small: sqlx's defaults (10) run out
/// quickly and the error appears only under load. Override via
/// `APP_DB_MAX_CONNECTIONS` based on measurement.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 5;

/// Maximum time to obtain a connection from the pool. Without this, an
/// exhausted pool hangs the request forever instead of failing.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    let options: PgConnectOptions = database_url.parse()?;

    PgPoolOptions::new()
        .max_connections(max_connections)
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

/// The constraint that rejected the row, when the error came from the database.
pub(crate) fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}
