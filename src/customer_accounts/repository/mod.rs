//! Customer accounts data access.
//!
//! Two sums carry this module: the balance is `SUM(amount)` over the entries,
//! and a sale's outstanding amount is its charge minus the notes that cancelled
//! part of it minus the receipts allocated to it. Neither is stored, so neither
//! can drift.
//!
//! `unapplied_credit` is the third: money collected, not allocated to a sale and
//! not handed back. It is what an operator can still apply, and the reason a
//! customer can owe on one sale while holding money for the next.
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

    #[error("customer not found")]
    CustomerNotFound,

    #[error("sale not found")]
    SaleNotFound,

    #[error("account entry not found")]
    EntryNotFound,

    #[error("that sale belongs to another customer")]
    SaleOfAnotherCustomer,

    #[error("that sale is not completed")]
    SaleNotChargeable,

    #[error("this sale is already charged to the account")]
    AlreadyCharged,

    #[error("the amount must be positive")]
    NonPositiveAmount,

    #[error("the amount {amount} carries more than two decimals")]
    TooManyDecimals { amount: Decimal },

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("the allocation of {requested} exceeds the {outstanding} this sale still owes")]
    OutstandingExceeded {
        requested: Decimal,
        outstanding: Decimal,
    },

    #[error("the allocations ({allocated}) exceed the money the receipt collected ({collected})")]
    ReceiptOverAllocated {
        allocated: Decimal,
        collected: Decimal,
    },

    #[error("this receipt collected {collected} and {allocated} is already allocated")]
    AllocationExceedsReceipt {
        collected: Decimal,
        allocated: Decimal,
    },

    #[error("the allocation of {requested} exceeds the {available} the customer holds in credit")]
    AllocationExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("only a receipt can be allocated to a sale")]
    NotAReceipt,

    #[error("wallet movement not found")]
    MovementNotFound,

    #[error("the receipt records {receipt} but the wallet movement carries {movement}")]
    ReceiptDoesNotMatchMovement { movement: Decimal, receipt: Decimal },

    #[error("the refund of {requested} exceeds the {available} the customer holds in credit")]
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

    #[error("the charge of {requested} exceeds the {available} the sale leaves unpaid")]
    ChargeExceedsSaleTotal {
        requested: Decimal,
        available: Decimal,
    },

    #[error("credit note not found")]
    CreditNoteNotFound,

    #[error("that credit note belongs to another sale")]
    CreditNoteMismatch,

    #[error("the note cancels {requested} but the note is only {available}")]
    CreditNoteExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the same sale appears twice in one receipt")]
    DuplicateAllocation { sale_id: Uuid },

    #[error("an account entry cannot be backed by a {kind} movement")]
    MovementNotFromAnAccount { kind: String },

    #[error("the wallet movement belongs to another customer")]
    MovementOfAnotherCustomer,

    #[error("the customer account is busy, retry")]
    CustomerBusy,

    #[error("the customer is deactivated")]
    CustomerInactive,

    #[error("the account would reach {requested} and only {available} is available")]
    CreditLimitExceeded {
        requested: Decimal,
        available: Decimal,
    },

    /// The caps inside `apply_available_credit` re-derive everything the
    /// ledger's own guards check, so a guard firing there is an impossible
    /// ledger violation, not a client error. Typed so the completion can
    /// answer an internal error without pretending a database protocol
    /// failed.
    #[error("the credit application disagreed with the ledger's own guards")]
    CreditApplicationBroken,
}

/// A collection: the movement that carried the money, and the sales it pays.
///
/// `allocations` is what the operator decided, never something the server
/// invents; the remainder of `amount` carries no row anywhere and that is
/// exactly what an available credit is.
pub struct NewReceipt<'a> {
    pub customer_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}

/// The receipt as recorded, or as it already was: a replay answers the first
/// entry so a retried collection never collects twice.
pub struct RecordedReceipt {
    pub entry_id: Uuid,
    pub replayed: bool,
}

/// The receipt a replay must answer with: the entry as it already was, the
/// movement that carried its money, and the sales its allocations pay —
/// everything the response needs, so a replay never re-derives anything from
/// the request that retried it.
pub struct ExistingReceipt {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub sale_ids: Vec<Uuid>,
}

/// The credit refund an idempotency key already recorded, or none.
pub struct ExistingCreditRefund {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
}

/// Input for recording a credit refund with an optional idempotency key.
pub struct NewCreditRefund<'a> {
    pub customer_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}
