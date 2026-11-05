//! Conexión a Postgres y la única consulta de esta rebanada (liveness de la DB).

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, postgres::PgConnectOptions};
use std::time::Duration;

/// Tamaño del pool. Arranca chico a propósito: los defaults de sqlx (10) se
/// agotan rápido y el error aparece recién bajo carga. Ajustar por medición.
const POOL_MAX_CONNECTIONS: u32 = 5;

/// Tiempo máximo para obtener una conexión del pool. Sin esto, un pool agotado
/// cuelga la request para siempre en vez de fallar.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let options: PgConnectOptions = database_url.parse()?;

    PgPoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
}

/// Readiness: la DB responde. Se usa en `/readyz`.
///
/// `SELECT 1` no prueba que las migraciones estén aplicadas; prueba que el
/// pool puede hablar con Postgres. Es lo que necesita un readiness probe.
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
