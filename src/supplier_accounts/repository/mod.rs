//! Supplier accounts data access.
//!
//! Mirrors [`crate::customer_accounts::repository`]:
//! - `balance` is `SUM(amount)` over `supplier_account_entries` (positive = we
//!   owe the supplier, negative = advance credit in our favour);
//! - `outstanding(purchase)` is `purchase_charge - purchase_return - SUM(allocations)`;
//! - `unapplied_credit` is `(payment + purchase_return_credit) - refunded - allocated >= 0`.
//!
//! Concurrency: every balance-changing writer calls [`lock_supplier`] first,
//! acquiring `pg_advisory_xact_lock(hashtextextended(supplier_id, 1))` with a
//! 5-second local lock timeout before locking any `suppliers` row `FOR UPDATE`.
//! Using seed `1` keeps the advisory lock namespace disjoint from
//! `customer_accounts` (which uses seed `0`).
//!
//! The reads live in `reads`, the writers in `writes`; both halves are
//! re-exported below, so every caller keeps the `repository::` path it
//! always used.

mod reads;
mod writes;

pub use reads::*;
pub use writes::*;

use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error("supplier not found")]
    SupplierNotFound,

    #[error("purchase not found")]
    PurchaseNotFound,

    #[error("account entry not found")]
    EntryNotFound,

    #[error("that purchase belongs to another supplier")]
    PurchaseOfAnotherSupplier,

    #[error("that purchase is not completed")]
    PurchaseNotChargeable,

    #[error("this purchase is already charged to the account")]
    AlreadyCharged,

    #[error("the amount must be positive")]
    NonPositiveAmount,

    #[error("the amount {amount} carries more than two decimals")]
    TooManyDecimals { amount: Decimal },

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("the allocation of {requested} exceeds the {outstanding} this purchase still owes")]
    OutstandingExceeded {
        requested: Decimal,
        outstanding: Decimal,
    },

    #[error("the allocations ({allocated}) exceed the money paid ({paid})")]
    PaymentOverAllocated { allocated: Decimal, paid: Decimal },

    #[error("this payment paid {paid} and {allocated} is already allocated")]
    AllocationExceedsPayment { paid: Decimal, allocated: Decimal },

    #[error("the allocation of {requested} exceeds the {available} held in supplier credit")]
    AllocationExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("only a payment can be allocated to a purchase")]
    NotAPayment,

    #[error("wallet movement not found")]
    MovementNotFound,

    #[error("the entry records {entry} but the wallet movement carries {movement}")]
    EntryDoesNotMatchMovement { movement: Decimal, entry: Decimal },

    #[error("the refund of {requested} exceeds the {available} held in supplier credit")]
    CreditRefundExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("an adjustment cannot cross zero: the balance is {balance}")]
    AdjustmentCrossesZero { balance: Decimal },

    #[error("that kind is not an adjustment")]
    NotAnAdjustment,

    #[error("an adjustment needs a reason")]
    AdjustmentNoteRequired,

    #[error("that document is already recorded in the ledger")]
    AlreadyRecorded,

    #[error("the charge of {requested} exceeds the {available} the purchase leaves unpaid")]
    ChargeExceedsPurchaseTotal {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the same purchase appears twice in one payment")]
    DuplicateAllocation { purchase_id: Uuid },

    #[error("a supplier account entry cannot be backed by a {kind} movement")]
    MovementNotFromSupplierAccount { kind: String },

    #[error("purchase return not found")]
    PurchaseReturnNotFound,

    #[error("that purchase return belongs to another purchase")]
    PurchaseReturnMismatch,

    #[error("the return of {requested} exceeds the {available} this purchase return carries")]
    PurchaseReturnExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the wallet movement belongs to another supplier")]
    MovementOfAnotherSupplier,

    #[error("the supplier account is busy, retry")]
    SupplierBusy,

    #[error("the supplier is deactivated")]
    SupplierInactive,

    #[error("the account would reach {requested} and only {available} is available")]
    CreditLimitExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the credit application disagreed with the ledger's own guards")]
    CreditApplicationBroken,
}

/// A supplier payment: the wallet outflow that moved the money, and the
/// completed purchases the operator chose to allocate it to.
pub struct NewSupplierPayment<'a> {
    pub supplier_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}

pub struct RecordedSupplierPayment {
    pub entry_id: Uuid,
    pub replayed: bool,
}

/// An already-recorded supplier payment for idempotent replay.
pub struct ExistingSupplierPayment {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub purchase_ids: Vec<Uuid>,
}

/// An already-recorded supplier credit refund for idempotent replay.
pub struct ExistingSupplierCreditRefund {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
}

pub struct NewSupplierCreditRefund<'a> {
    pub supplier_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}
