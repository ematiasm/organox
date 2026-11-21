//! Customer types.
//!
//! Layer decision: a customer carries no derived state. What a customer bought,
//! owes or returned is read from `sales`, `credit_notes` and the wallets, never
//! stored twice here. The two stored values that are decisions rather than
//! flags are `is_active`, which exists so a customer can stop being offered
//! without leaving the history, and `credit_limit`, the operator's ceiling on
//! what the customer may owe — a policy, never a balance.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
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
    /// The ceiling on what this customer may owe. `NULL` (the default) means
    /// no limit, `0` means cash only; the balance itself lives in the account
    /// ledger and is never stored here.
    pub credit_limit: Option<Decimal>,
    /// Derived at read time from `customer_account_entries`, never stored on
    /// `customers`. Positive: the customer owes; negative: credit in their
    /// favour.
    pub balance: Decimal,
    /// Derived at read time from `customer_account_entries` and
    /// `customer_account_allocations`, never stored on `customers`. Money
    /// collected that no sale has consumed and no refund has returned (`U`).
    pub available_credit: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
