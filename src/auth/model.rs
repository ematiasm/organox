//! Authentication domain types.
//!
//! Layer rule: `PasswordHash` and `Role` are constructed only through validated
//! paths (`PasswordHash::from_phc`, `Role` comes from the DB), and external
//! serialization uses explicit DTOs, never these structs.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Already verified password hash in PHC format (argon2id).
///
/// A deliberate newtype: it cannot be constructed from an arbitrary `String`,
/// only from the hasher output or a DB value already satisfying the
/// `CHECK (password_hash LIKE '$argon2%')` constraint.
#[derive(Clone)]
pub struct PasswordHash(String);

impl PasswordHash {
    pub(crate) fn from_phc(phc: String) -> Self {
        debug_assert!(phc.starts_with("$argon2"), "invalid PHC: {phc}");
        Self(phc)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Never logged or serialized. `Debug` is deliberately redacted.
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
    Manager,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Manager => "manager",
        }
    }

    /// The ONLY point of role-based authorization.
    ///
    /// Today, with two roles and no permissions crossing roles, this is a
    /// boolean `match`. When the case "warehouse staff add stock but cannot
    /// see prices" arises, add `permissions`/`role_permissions` and change ONLY
    /// the body of this function (and the other rules): handlers remain untouched.
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
            "manager" => Ok(Role::Manager),
            other => Err(format!(
                "unknown role: {other:?} (valid roles: owner, manager)"
            )),
        }
    }
}

/// User as stored in the database. Never serialized directly to HTTP.
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

/// Session lifetime. Sliding: each valid request extends it.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7); // 7 days

/// Absolute maximum lifetime from creation, even if used every day.
/// Without this cap, a stolen cookie remains valid forever if the attacker
/// uses it frequently.
pub const SESSION_MAX_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 30); // 30 days

/// Active session as seen from an already authenticated request.
#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
}
