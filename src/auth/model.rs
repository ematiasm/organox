//! Tipos del dominio de autenticación.
//!
//! Regla de esta capa: el `PasswordHash` y el `Role` se construyen solo por
//! caminos validados (`PasswordHash::from_phc`, `Role` viene de la DB), y el
//! serializado hacia afuera usa DTOs explícitos, nunca estas structs.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hash de contraseña ya verificado en formato PHC (argon2id).
///
/// Es un newtype a propósito: no se puede construir desde un `String` arbitrario,
/// solo desde la salida del hasher o desde un valor de la DB que ya cumple la
/// restricción `CHECK (password_hash LIKE '$argon2%')`.
#[derive(Clone)]
pub struct PasswordHash(String);

impl PasswordHash {
    pub(crate) fn from_phc(phc: String) -> Self {
        debug_assert!(phc.starts_with("$argon2"), "PHC inválido: {phc}");
        Self(phc)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Nunca se loguea ni se serializa. `Debug` redactado a propósito.
impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash(<redacted>)")
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Encargado,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Encargado => "encargado",
        }
    }

    /// Punto ÚNICO de autorización por rol.
    ///
    /// Hoy con dos roles y sin permisos que crucen roles, esto es un `match`
    /// booleano. Cuando aparezca el caso "deposito carga stock pero no ve
    /// precios", se agrega `permissions`/`role_permissions` y se cambia SOLO
    /// el cuerpo de esta función (y la de las demás reglas): los handlers no
    /// se tocan.
    pub fn can_manage_users(self) -> bool {
        matches!(self, Role::Owner)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "owner" => Ok(Role::Owner),
            "encargado" => Ok(Role::Encargado),
            other => Err(format!(
                "rol desconocido: {other:?} (válidos: owner, encargado)"
            )),
        }
    }
}

/// Usuario tal como vive en la base. Nunca se serializa directo a HTTP.
#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    pub fn can_manage_users(&self) -> bool {
        self.role.can_manage_users()
    }
}

/// Vida útil de una sesión. Deslizante: cada request válida la extiende.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7); // 7 días

/// Vida útil máxima absoluta desde la creación, aunque se use todos los días.
/// Sin este tope, una cookie robada es válida para siempre si el atacante la
/// usa seguido.
pub const SESSION_MAX_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 30); // 30 días

/// Sesión activa vista desde un request ya autenticado.
#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
}
