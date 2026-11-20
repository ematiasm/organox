//! Customer accounts: the debt a customer carries.
//!
//! Layer decision, the mirror of the wallet one: a customer has **no** `balance`
//! field and a sale has **no** `paid` field. Both are derived — the balance from
//! `customer_account_entries`, a sale's outstanding amount from its charge minus
//! the notes that cancelled part of it minus the receipts allocated to it. A
//! stored copy drifts the first time a transaction forgets to update it.
//!
//! Two quantities describe an account, and they are not the same thing:
//!
//! - the **balance** is what the customer owes (positive) or holds in their
//!   favour (negative);
//! - the **unapplied credit** is money actually collected that no sale has been
//!   charged against yet. A customer can owe on one sale and hold money for the
//!   next one, and only this number says how much of the credit is usable.
//!
//! The invariant that ties both books together lives in
//! [`AccountPosition::is_consistent`].

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Why the customer's account moved.
///
/// An enum, like `WalletMovementKind`, for the same reason: a new kind always
/// arrives with a writer, a validation and a screen — that is, with code.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "customer_account_entry_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CustomerAccountEntryKind {
    /// A sale left part of its total owed.
    SaleCharge,
    /// Money collected against the account.
    Receipt,
    /// A credit note cancelled part of a sale's debt.
    CreditNote,
    /// Money handed back against a credit balance.
    CreditRefund,
    /// Manual increase: an opening balance, or a correction.
    AdjustmentIn,
    /// Manual decrease: a correction.
    AdjustmentOut,
}

impl CustomerAccountEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CustomerAccountEntryKind::SaleCharge => "sale_charge",
            CustomerAccountEntryKind::Receipt => "receipt",
            CustomerAccountEntryKind::CreditNote => "credit_note",
            CustomerAccountEntryKind::CreditRefund => "credit_refund",
            CustomerAccountEntryKind::AdjustmentIn => "adjustment_in",
            CustomerAccountEntryKind::AdjustmentOut => "adjustment_out",
        }
    }

    /// The direction this kind moves the balance. The stored `amount` already
    /// carries this sign — a single signed column, unlike the wallet ledger,
    /// where the sign lives in the kind and every `CASE` has to repeat it — so
    /// this exists to build the amount, never to interpret it.
    pub fn sign(self) -> i8 {
        match self {
            CustomerAccountEntryKind::SaleCharge
            | CustomerAccountEntryKind::CreditRefund
            | CustomerAccountEntryKind::AdjustmentIn => 1,
            CustomerAccountEntryKind::Receipt
            | CustomerAccountEntryKind::CreditNote
            | CustomerAccountEntryKind::AdjustmentOut => -1,
        }
    }

    /// Kinds an operator records by hand. A charge belongs to the completion of
    /// a sale, a receipt to a collection, and a note to the credit note that
    /// produced it: none of them is ever typed into a form.
    pub fn is_manual(self) -> bool {
        matches!(
            self,
            CustomerAccountEntryKind::AdjustmentIn | CustomerAccountEntryKind::AdjustmentOut
        )
    }
}

impl fmt::Display for CustomerAccountEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CustomerAccountEntryKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "sale_charge" => Ok(CustomerAccountEntryKind::SaleCharge),
            "receipt" => Ok(CustomerAccountEntryKind::Receipt),
            "credit_note" => Ok(CustomerAccountEntryKind::CreditNote),
            "credit_refund" => Ok(CustomerAccountEntryKind::CreditRefund),
            "adjustment_in" => Ok(CustomerAccountEntryKind::AdjustmentIn),
            "adjustment_out" => Ok(CustomerAccountEntryKind::AdjustmentOut),
            other => Err(format!(
                "unknown kind: {other:?} (valid: sale_charge, receipt, credit_note, credit_refund, adjustment_in, adjustment_out)"
            )),
        }
    }
}

/// One line of the ledger. Append-only: a mistake is corrected with a
/// compensating entry, never by editing this row.
#[derive(Debug, Clone)]
pub struct CustomerAccountEntry {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub kind: CustomerAccountEntryKind,
    /// Signed: positive grows the debt, negative shrinks it.
    pub amount: Decimal,
    pub sale_id: Option<Uuid>,
    pub credit_note_id: Option<Uuid>,
    pub wallet_movement_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// How much of a receipt pays which sale.
#[derive(Debug, Clone)]
pub struct CustomerAccountAllocation {
    pub id: Uuid,
    pub entry_id: Uuid,
    pub sale_id: Uuid,
    pub amount: Decimal,
    pub created_at: DateTime<Utc>,
}

/// A sale that still owes money, with the three numbers that produce the debt.
///
/// Kept apart instead of a single `outstanding` so a statement can explain the
/// number instead of asserting it: what was charged, what a note cancelled, and
/// what the receipts paid.
#[derive(Debug, Clone)]
pub struct SaleOutstanding {
    pub sale_id: Uuid,
    pub completed_at: DateTime<Utc>,
    pub charged: Decimal,
    pub cancelled: Decimal,
    pub allocated: Decimal,
    pub outstanding: Decimal,
}

/// Everything the account says about a customer at a point in time.
#[derive(Debug, Clone)]
pub struct AccountPosition {
    /// Positive: the customer owes. Negative: the customer holds credit.
    pub balance: Decimal,
    /// Money collected that no sale has been charged against yet. Never
    /// negative, and never more than `-balance`.
    pub unapplied_credit: Decimal,
    /// Sum of what every sale still owes.
    pub outstanding_total: Decimal,
    /// Signed sum of the manual adjustments. They move the balance without
    /// belonging to any sale, so the invariant has to name them separately.
    pub adjustments: Decimal,
}

impl AccountPosition {
    /// The one identity the two books must always satisfy.
    ///
    /// `balance = outstanding - unapplied credit + adjustments`, rearranged to
    /// avoid a subtraction: a customer who owes 100 and holds 40 in credit has a
    /// balance of 60 and an outstanding total of 100.
    pub fn is_consistent(&self) -> bool {
        self.balance + self.unapplied_credit == self.outstanding_total + self.adjustments
    }
}
