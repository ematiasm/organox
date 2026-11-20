//! Customer types.
//!
//! Layer decision: a customer carries no derived state. What a customer bought,
//! owes or returned is read from `sales`, `credit_notes` and the wallets, never
//! stored twice here. The only flag is `is_active`, which exists so a customer
//! can stop being offered without leaving the history.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Customer as stored in the DB.
#[derive(Debug, Clone)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    /// Optional document. Unique when present, compared without case.
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
