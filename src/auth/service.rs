//! Casos de uso de autenticación.
//!
//! Capa entre el borde HTTP y el repositorio. Responsabilidades:
//!  * decidir (política de login, emisión de sesión, autorización).
//!  * mover el trabajo costoso de argon2 fuera del runtime de tokio.
//!  * no filtrar información: el mismo resultado para "email inexistente" y
//!    "contraseña incorrecta".

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::OnceLock;
use uuid::Uuid;

use crate::auth::model::{PasswordHash, Role, User};
use crate::auth::password::{self, PasswordError};
use crate::auth::repository::{self, NewUser, RepoError};
use crate::auth::token::{SessionToken, TokenHash};
use crate::auth::validation::{ValidatedEmail, ValidationError};
use crate::ratelimit::{AccountLimiter, Decision};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("credenciales inválidas")]
    InvalidCredentials,

    #[error("el usuario no tiene permiso para esta operación")]
    Forbidden,

    /// Cuenta temporalmente bloqueada por intentos fallidos.
    /// Es un error distinto de `InvalidCredentials` a propósito: el cliente
    /// necesita poder decirle al usuario que espere, en vez de "contraseña
    /// incorrecta" (que lo llevaría a reintentar y extender el bloqueo).
    #[error("cuenta bloqueada temporalmente hasta {until}")]
    AccountLocked { until: DateTime<Utc> },

    #[error("el email ya está registrado")]
    EmailTaken,

    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("error interno de autenticación")]
    Internal(#[from] RepoError),
}

impl From<sqlx::Error> for AuthError {
    fn from(err: sqlx::Error) -> Self {
        AuthError::Internal(RepoError::Database(err))
    }
}

impl From<PasswordError> for AuthError {
    fn from(err: PasswordError) -> Self {
        match err {
            PasswordError::Invalid(v) => AuthError::Validation(v),
            other => {
                // Un fallo del hasher es un bug o una dependencia rota, no una
                // credencial inválida. Se registra y se devuelve error interno.
                tracing::error!(error = %other, "fallo de hashing/verificación");
                AuthError::Internal(RepoError::Crypto)
            }
        }
    }
}

/// Sesión emitida. El token en claro se entrega **una sola vez**, para la
/// cookie; después solo existe su hash.
pub struct IssuedSession {
    pub token: SessionToken,
    pub expires_at: DateTime<Utc>,
}

pub enum LoginOutcome {
    Authenticated {
        user: User,
        session: IssuedSession,
    },
    InvalidCredentials,
    /// Bloqueada por fuerza bruta: no se evaluaron credenciales.
    Locked {
        until: DateTime<Utc>,
    },
}

/// Crea un usuario. Exige que el actor pueda gestionar usuarios.
///
/// El rol del actor se pasa explícito porque la única autoridad sobre "quién
/// hace esta request" es el extractor de autenticación, no este módulo.
pub async fn create_user(
    pool: &PgPool,
    actor_role: Role,
    email: &str,
    password_plain: &str,
    role: Role,
) -> Result<Uuid, AuthError> {
    if !actor_role.can_manage_users() {
        return Err(AuthError::Forbidden);
    }

    let email = ValidatedEmail::parse(email)?;

    let id = repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash_off_thread(password_plain.to_string()).await?,
            role,
        },
    )
    .await
    .map_err(|err| match err {
        RepoError::EmailTaken => AuthError::EmailTaken,
        other => AuthError::Internal(other),
    })?;

    Ok(id)
}

/// Verifica credenciales y, si son válidas, emite una sesión nueva.
///
/// Dos detalles que importan y que se pierden en casi todas las
/// implementaciones:
///
///  1. Si el email no existe, igual se gasta un hash. Sin esto, la latencia
///     distingue "email no registrado" de "contraseña incorrecta" y el login
///     pasa a ser un oráculo para enumerar usuarios.
///  2. `InvalidCredentials` es el MISMO valor en ambos casos.
pub async fn login(
    pool: &PgPool,
    limiter: &AccountLimiter,
    email: &str,
    password_plain: &str,
    user_agent: Option<&str>,
) -> Result<LoginOutcome, AuthError> {
    let Ok(email) = ValidatedEmail::parse(email) else {
        // Un email malformado no es una credencial válida; no se revela por qué.
        return Ok(LoginOutcome::InvalidCredentials);
    };

    // El chequeo de bloqueo va ANTES de hashear: si la cuenta está bloqueada,
    // no hay razón para gastar 400ms de argon2 ni para consultar el usuario.
    // Esto también cierra la puerta a usar el login como amplificador de CPU.
    if let Decision::Blocked { until } = limiter.check(&email).await? {
        return Ok(LoginOutcome::Locked { until });
    }

    let Some(found) = repository::find_user_by_email(pool, &email).await? else {
        // Mismo costo que un login real, mismo resultado. El intento cuenta
        // igual: si no contara, un atacante podría distinguir emails
        // registrados por la diferencia entre "bloqueado" y "401".
        limiter.record_failure(&email).await?;
        let dummy = dummy_hash();
        let plain = password_plain.to_string();
        let _ = tokio::task::spawn_blocking(move || password::verify(&plain, &dummy)).await;
        return Ok(LoginOutcome::InvalidCredentials);
    };

    let stored = found.password_hash.clone();
    let plain = password_plain.to_string();
    let matches = tokio::task::spawn_blocking(move || password::verify(&plain, &stored))
        .await
        .map_err(|_| AuthError::Internal(RepoError::Crypto))??;

    if !matches {
        // Se registra la falla, pero la respuesta sigue siendo 401: la
        // contraseña SÍ era incorrecta, y decirle al usuario "demasiados
        // intentos" en el mismo intento que lo bloqueó es mentirle.
        //
        // El bloqueo rige desde el intento SIGUIENTE, que es el que devuelve
        // 429. Así `max_failures = 3` significa: 3 contraseñas incorrectas, y
        // la cuarta ni se evalúa.
        if let Decision::Blocked { until } = limiter.record_failure(&email).await? {
            tracing::warn!(
                email = %email,
                blocked_until = %until,
                "cuenta bloqueada por intentos fallidos; aplica desde el próximo intento"
            );
        }
        return Ok(LoginOutcome::InvalidCredentials);
    }

    // Login correcto: es prueba de que no es un atacante, se limpia el conteo.
    // Sin esto, alguien que se equivocó 5 veces y después recordó su
    // contraseña queda bloqueado igual.
    limiter.record_success(&email).await?;

    let token = SessionToken::generate();
    let expires_at = repository::create_session(pool, &token.hash(), found.id, user_agent).await?;

    Ok(LoginOutcome::Authenticated {
        user: User {
            id: found.id,
            email: email.as_str().to_string(),
            role: found.role,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
        session: IssuedSession { token, expires_at },
    })
}

/// Resuelve el token de una cookie a un usuario autenticado.
pub async fn authenticate(
    pool: &PgPool,
    token: &SessionToken,
) -> Result<Option<AuthenticatedUser>, AuthError> {
    let found = repository::find_session_user(pool, &token.hash()).await?;

    Ok(found.map(|u| AuthenticatedUser {
        id: u.id,
        email: u.email,
        role: u.role,
        token_hash: token.hash(),
    }))
}

/// Extiende la sesión usada. Best-effort: nunca hace fallar la request.
pub async fn touch(pool: &PgPool, token: &SessionToken) {
    repository::touch_session(pool, &token.hash()).await;
}

/// Cierra la sesión. Idempotente: borrar una sesión inexistente no es error.
pub async fn logout(pool: &PgPool, token: &SessionToken) -> Result<(), AuthError> {
    repository::delete_session(pool, &token.hash()).await?;
    Ok(())
}

/// Logout cuando ya se tiene el hash del token (lo normal: viene del extractor).
pub async fn logout_by_hash(pool: &PgPool, token_hash: &TokenHash) -> Result<(), AuthError> {
    repository::delete_session(pool, token_hash).await?;
    Ok(())
}

/// Usuario ya autenticado en esta request. Lleva el hash del token para que el
/// logout no tenga que releer la cookie.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub token_hash: TokenHash,
}

impl AuthenticatedUser {
    pub fn can_manage_users(&self) -> bool {
        self.role.can_manage_users()
    }
}

async fn hash_off_thread(plain: String) -> Result<PasswordHash, AuthError> {
    tokio::task::spawn_blocking(move || password::hash(&plain))
        .await
        .map_err(|_| AuthError::Internal(RepoError::Crypto))?
        .map_err(AuthError::from)
}

/// Precalcula el hash señuelo.
///
/// Sin esto, el primer login con email inexistente paga además el costo de
/// generarlo, y la diferencia de latencia lo delata como oráculo de
/// enumeración de usuarios. Se llama una vez al arrancar el servidor.
pub fn warm_dummy_hash() {
    let _ = dummy_hash();
}

/// Hash de relleno, calculado una sola vez por proceso.
fn dummy_hash() -> PasswordHash {
    static DUMMY: OnceLock<Option<PasswordHash>> = OnceLock::new();

    DUMMY
        .get_or_init(|| password::hash("relleno-solo-para-gastar-tiempo-de-hash").ok())
        .clone()
        .unwrap_or_else(|| {
            // Si el hasher no está disponible, cualquier hash es tan inútil
            // como el otro: lo único que importa es no cortar el camino antes.
            PasswordHash::from_phc(
                "$argon2id$v=19$m=19456,t=2,p=1$MDAwMDAwMDAwMDAwMDAwMA$0000000000000000000000000000000000000000000"
                    .to_string(),
            )
        })
}
