//! Supplier types.
//!
//! `balance` and `available_credit` are never stored on the `suppliers` table;
//! they are derived on read from `supplier_account_entries` and
//! `supplier_account_allocations`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Supplier as stored in the DB, enriched on read with its derived account
/// position.
#[derive(Debug, Clone)]
pub struct Supplier {
    pub id: Uuid,
    pub name: String,
    /// Optional document. Unique when present, compared without case.
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    /// `None` = unlimited, `Some(0)` = cash only, `Some(x)` = exposure cap.
    pub credit_limit: Option<Decimal>,
    /// Derived signed sum of `supplier_account_entries` (+ = we owe, - = credit).
    pub balance: Decimal,
    /// Derived unapplied advance/credit paid to this supplier (`U >= 0`).
    pub available_credit: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
