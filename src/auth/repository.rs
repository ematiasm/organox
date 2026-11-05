//! Acceso a datos de autenticación.
//!
//! Todo el SQL de auth vive acá. Las queries usan las macros de `sqlx` para que
//! el compilador verifique tipos y nombres de columnas contra la DB real.
//!
//! Consecuencia ineludible: para compilar hace falta una DB alcanzable con
//! `DATABASE_URL`, o el cache de `cargo sqlx prepare` commiteado en `.sqlx/`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{PasswordHash, Role, SESSION_MAX_LIFETIME, SESSION_TTL};
use super::token::TokenHash;
use super::validation::ValidatedEmail;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("el email ya está registrado")]
    EmailTaken,

    /// Fallo criptográfico (hasher no disponible, hash corrupto). Es un bug o
    /// una dependencia rota, nunca una credencial inválida.
    #[error("fallo criptográfico")]
    Crypto,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

fn is_email_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("users_email_unique"))
}

pub struct NewUser<'a> {
    pub email: &'a ValidatedEmail,
    pub password_hash: &'a PasswordHash,
    pub role: Role,
}

pub struct FoundUser {
    pub id: Uuid,
    pub password_hash: PasswordHash,
    pub role: Role,
}

pub struct FoundSession {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
}

/// El id es uuid v7 generado en la app: ordenable por tiempo y no predecible.
pub async fn create_user(pool: &PgPool, new: NewUser<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO users (id, email, password_hash, role)
        VALUES ($1, $2, $3, $4)
        "#,
        id,
        new.email.as_str(),
        new.password_hash.as_str(),
        new.role as Role,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if is_email_unique_violation(&err) {
            RepoError::EmailTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

pub async fn find_user_by_email(
    pool: &PgPool,
    email: &ValidatedEmail,
) -> Result<Option<FoundUser>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id, password_hash, role AS "role: Role"
        FROM users
        WHERE email = $1
        "#,
        email.as_str()
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| FoundUser {
        id: r.id,
        password_hash: PasswordHash::from_phc(r.password_hash),
        role: r.role,
    }))
}

/// Emite una sesión. `expires_at` se calcula en la DB para no depender del reloj
/// de la app (que puede estar desincronizado respecto del de Postgres).
pub async fn create_session(
    pool: &PgPool,
    token_hash: &TokenHash,
    user_id: Uuid,
    user_agent: Option<&str>,
) -> Result<DateTime<Utc>, RepoError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO sessions (token_hash, user_id, user_agent, expires_at)
        VALUES ($1, $2, $3, now() + make_interval(secs => $4))
        RETURNING expires_at
        "#,
        token_hash.as_bytes(),
        user_id,
        user_agent,
        SESSION_TTL.as_secs_f64(),
    )
    .fetch_one(pool)
    .await?;

    Ok(row.expires_at)
}

/// Resuelve token → usuario en una consulta. `None` cubre los tres casos en que
/// el cliente debe reautenticarse: token inexistente, sesión expirada, usuario
/// borrado.
pub async fn find_session_user(
    pool: &PgPool,
    token_hash: &TokenHash,
) -> Result<Option<FoundSession>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT u.id, u.email::text AS "email!", u.role AS "role: Role"
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1
          AND s.expires_at > now()
        "#,
        token_hash.as_bytes()
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| FoundSession {
        id: r.id,
        email: r.email,
        role: r.role,
    }))
}

/// Extiende la sesión usada, respetando el tope absoluto desde `created_at`:
/// una cookie robada no vive para siempre por usarse seguido.
///
/// Best-effort: un fallo se loguea y no afecta la request.
pub async fn touch_session(pool: &PgPool, token_hash: &TokenHash) {
    let result = sqlx::query!(
        r#"
        UPDATE sessions
        SET last_used_at = now(),
            expires_at = LEAST(
                now() + make_interval(secs => $2),
                created_at + make_interval(secs => $3)
            )
        WHERE token_hash = $1
          AND expires_at > now()
        "#,
        token_hash.as_bytes(),
        SESSION_TTL.as_secs_f64(),
        SESSION_MAX_LIFETIME.as_secs_f64(),
    )
    .execute(pool)
    .await;

    if let Err(err) = result {
        tracing::warn!(error = %err, "no se pudo extender la sesión");
    }
}

pub async fn delete_session(pool: &PgPool, token_hash: &TokenHash) -> Result<bool, RepoError> {
    let result = sqlx::query!(
        "DELETE FROM sessions WHERE token_hash = $1",
        token_hash.as_bytes()
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Limpieza de sesiones vencidas. Devuelve cuántas borró.
pub async fn purge_expired_sessions(pool: &PgPool) -> Result<u64, RepoError> {
    let result = sqlx::query!("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}
