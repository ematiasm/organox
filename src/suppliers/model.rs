//! Supplier types.
//!
//! A supplier carries no derived balance or purchase totals here; purchases
//! live in their own module and never duplicate state onto the registry row.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Supplier as stored in the DB.
#[derive(Debug, Clone)]
pub struct Supplier {
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
