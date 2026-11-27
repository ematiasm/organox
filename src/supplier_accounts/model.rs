//! Supplier accounts: payables and advance credit with a supplier.
//!
//! Mirrors [`crate::customer_accounts::model`]: neither `suppliers` nor
//! `purchases` stores a balance or paid flag. Every balance is the signed sum
//! of `supplier_account_entries`, and every purchase's outstanding payable is
//! its `purchase_charge` minus `purchase_return` cancellations and the
//! `supplier_account_allocations` pointing at it.
//!
//! Two quantities describe a supplier account:
//!
//! - the **balance** is what the business owes the supplier (positive) or
//!   advance credit held with the supplier (negative);
//! - the **unapplied credit** is money paid to the supplier that has not been
//!   allocated to a purchase or refunded back yet.
//!
//! The invariant tying both books together lives in
//! [`SupplierAccountPosition::is_consistent`].

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Why the supplier's account moved.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "supplier_account_entry_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SupplierAccountEntryKind {
    /// A purchase left part of its total on account.
    PurchaseCharge,
    /// Money paid against the supplier account.
    Payment,
    /// Returned merchandise cancelled part or all of an open purchase payable.
    PurchaseReturn,
    /// Paid surplus from a purchase return kept as unapplied supplier credit.
    PurchaseReturnCredit,
    /// Unapplied advance/credit refunded back by the supplier.
    CreditRefund,
    /// Manual increase of payable (e.g. opening balance or correction).
    AdjustmentIn,
    /// Manual decrease of payable (correction).
    AdjustmentOut,
}

impl SupplierAccountEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SupplierAccountEntryKind::PurchaseCharge => "purchase_charge",
            SupplierAccountEntryKind::Payment => "payment",
            SupplierAccountEntryKind::PurchaseReturn => "purchase_return",
            SupplierAccountEntryKind::PurchaseReturnCredit => "purchase_return_credit",
            SupplierAccountEntryKind::CreditRefund => "credit_refund",
            SupplierAccountEntryKind::AdjustmentIn => "adjustment_in",
            SupplierAccountEntryKind::AdjustmentOut => "adjustment_out",
        }
    }

    /// Direction this kind moves the balance (positive grows what we owe).
    pub fn sign(self) -> i8 {
        match self {
            SupplierAccountEntryKind::PurchaseCharge
            | SupplierAccountEntryKind::CreditRefund
            | SupplierAccountEntryKind::AdjustmentIn => 1,
            SupplierAccountEntryKind::Payment
            | SupplierAccountEntryKind::PurchaseReturn
            | SupplierAccountEntryKind::PurchaseReturnCredit
            | SupplierAccountEntryKind::AdjustmentOut => -1,
        }
    }

    /// Kinds an operator records manually without a linked document or wallet
    /// movement.
    pub fn is_manual(self) -> bool {
        matches!(
            self,
            SupplierAccountEntryKind::AdjustmentIn | SupplierAccountEntryKind::AdjustmentOut
        )
    }
}

impl fmt::Display for SupplierAccountEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SupplierAccountEntryKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "purchase_charge" => Ok(SupplierAccountEntryKind::PurchaseCharge),
            "payment" => Ok(SupplierAccountEntryKind::Payment),
            "purchase_return" => Ok(SupplierAccountEntryKind::PurchaseReturn),
            "purchase_return_credit" => Ok(SupplierAccountEntryKind::PurchaseReturnCredit),
            "credit_refund" => Ok(SupplierAccountEntryKind::CreditRefund),
            "adjustment_in" => Ok(SupplierAccountEntryKind::AdjustmentIn),
            "adjustment_out" => Ok(SupplierAccountEntryKind::AdjustmentOut),
            other => Err(format!(
                "unknown kind: {other:?} (valid: purchase_charge, payment, purchase_return, purchase_return_credit, credit_refund, adjustment_in, adjustment_out)"
            )),
        }
    }
}

/// One append-only ledger line on a supplier account.
#[derive(Debug, Clone)]
pub struct SupplierAccountEntry {
    pub id: Uuid,
    pub supplier_id: Uuid,
    pub kind: SupplierAccountEntryKind,
    /// Signed: positive grows what we owe, negative shrinks it.
    pub amount: Decimal,
    pub purchase_id: Option<Uuid>,
    pub purchase_return_id: Option<Uuid>,
    pub wallet_movement_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// How much of a supplier payment applies to a specific completed purchase.
#[derive(Debug, Clone)]
pub struct SupplierAccountAllocation {
    pub id: Uuid,
    pub entry_id: Uuid,
    pub purchase_id: Uuid,
    pub amount: Decimal,
    pub created_at: DateTime<Utc>,
}

/// A completed purchase that still has an unpaid on-account balance.
#[derive(Debug, Clone)]
pub struct PurchaseOutstanding {
    pub purchase_id: Uuid,
    pub completed_at: DateTime<Utc>,
    pub charged: Decimal,
    pub cancelled: Decimal,
    pub allocated: Decimal,
    pub outstanding: Decimal,
}

/// Everything the account says about a supplier at a point in time.
#[derive(Debug, Clone)]
pub struct SupplierAccountPosition {
    /// Positive: we owe the supplier. Negative: we hold advance credit with them.
    pub balance: Decimal,
    /// Money paid to the supplier that has neither been allocated to a purchase
    /// nor refunded back yet. Never negative.
    pub unapplied_credit: Decimal,
    /// Sum of what every open purchase still owes.
    pub outstanding_total: Decimal,
    /// Signed sum of manual adjustments.
    pub adjustments: Decimal,
}

impl SupplierAccountPosition {
    /// Core payables identity plus the non-negative unapplied-credit floor:
    /// `balance + unapplied_credit == outstanding_total + adjustments` and
    /// `unapplied_credit >= 0`.
    pub fn is_consistent(&self) -> bool {
        self.balance + self.unapplied_credit == self.outstanding_total + self.adjustments
            && self.unapplied_credit >= Decimal::ZERO
    }
}

/// One ledger line paired with the supplier's running balance right after it.
#[derive(Debug, Clone)]
pub struct SupplierAccountStatementEntry {
    pub entry: SupplierAccountEntry,
    pub running_balance: Decimal,
}

/// Complete account statement for one supplier: entries oldest first, current
/// position, and every purchase with open payable.
#[derive(Debug, Clone)]
pub struct SupplierAccountStatement {
    pub supplier_id: Uuid,
    pub position: SupplierAccountPosition,
    pub entries: Vec<SupplierAccountStatementEntry>,
    /// Ledger entries in the whole statement, not just the ones on this page.
    pub entries_total: i64,
    pub outstanding_purchases: Vec<PurchaseOutstanding>,
}

/// One purchase allocation attached to a supplier payment in the write log.
#[derive(Debug, Clone)]
pub struct SupplierPaymentLogAllocation {
    pub purchase_id: Uuid,
    pub amount: Decimal,
    pub created_at: DateTime<Utc>,
}

/// One supplier payment recorded via `POST /suppliers/{id}/payments`, with its
/// linked wallet movement and allocations.
#[derive(Debug, Clone)]
pub struct SupplierPaymentLogItem {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub wallet_id: Uuid,
    pub payment_method_id: Option<Uuid>,
    /// Positive amount paid (the ledger stores `-amount`).
    pub amount: Decimal,
    /// Sum of allocations pointing at this payment.
    pub allocated_total: Decimal,
    pub allocations: Vec<SupplierPaymentLogAllocation>,
    /// Stored on the linked `wallet_movements` row.
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One manual payable adjustment recorded via `POST /suppliers/{id}/adjustments`.
#[derive(Debug, Clone)]
pub struct SupplierAdjustmentLogItem {
    pub entry_id: Uuid,
    pub kind: SupplierAccountEntryKind,
    /// Signed effect on the balance (`+` for `adjustment_in`, `-` for
    /// `adjustment_out`).
    pub amount: Decimal,
    pub note: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Open purchase payables split into four age buckets computed from each
/// purchase's `completed_at` timestamp.
#[derive(Debug, Clone, Default)]
pub struct AgingBuckets {
    pub days_0_30: Decimal,
    pub days_31_60: Decimal,
    pub days_61_90: Decimal,
    pub days_90_plus: Decimal,
}

impl AgingBuckets {
    pub fn add(&mut self, other: &AgingBuckets) {
        self.days_0_30 += other.days_0_30;
        self.days_31_60 += other.days_31_60;
        self.days_61_90 += other.days_61_90;
        self.days_90_plus += other.days_90_plus;
    }
}

/// One supplier with an open account position (`balance != 0` or
/// `unapplied_credit != 0`) and their open-purchase aging buckets.
#[derive(Debug, Clone)]
pub struct PayableSupplier {
    pub supplier_id: Uuid,
    pub supplier_name: String,
    pub tax_id: Option<String>,
    pub is_active: bool,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
    pub outstanding_total: Decimal,
    pub adjustments: Decimal,
    pub aging: AgingBuckets,
}

/// Portfolio-wide payables report across every supplier with an open account
/// position.
#[derive(Debug, Clone)]
pub struct PayablesSummary {
    pub total_balance: Decimal,
    pub total_unapplied_credit: Decimal,
    pub total_outstanding: Decimal,
    pub total_adjustments: Decimal,
    pub aging: AgingBuckets,
    pub items: Vec<PayableSupplier>,
    /// Suppliers in the whole report, not just the ones on this page.
    pub items_total: i64,
}

/// Portfolio totals of the payables report.
///
/// They always describe the whole report, never the page: `items_total` is how
/// many suppliers have an open position, and every amount is the sum over all
/// of them. Paging only chooses which suppliers come back with it.
#[derive(Debug, Clone, Default)]
pub struct PayablesTotals {
    pub items_total: i64,
    pub total_balance: Decimal,
    pub total_unapplied_credit: Decimal,
    pub total_outstanding: Decimal,
    pub total_adjustments: Decimal,
    pub aging: AgingBuckets,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn entry_kind_signs_and_manual_flags_match_spec() {
        assert_eq!(SupplierAccountEntryKind::PurchaseCharge.sign(), 1);
        assert_eq!(SupplierAccountEntryKind::CreditRefund.sign(), 1);
        assert_eq!(SupplierAccountEntryKind::AdjustmentIn.sign(), 1);
        assert_eq!(SupplierAccountEntryKind::Payment.sign(), -1);
        assert_eq!(SupplierAccountEntryKind::PurchaseReturn.sign(), -1);
        assert_eq!(SupplierAccountEntryKind::PurchaseReturnCredit.sign(), -1);
        assert_eq!(SupplierAccountEntryKind::AdjustmentOut.sign(), -1);

        assert!(!SupplierAccountEntryKind::PurchaseCharge.is_manual());
        assert!(!SupplierAccountEntryKind::Payment.is_manual());
        assert!(!SupplierAccountEntryKind::PurchaseReturn.is_manual());
        assert!(!SupplierAccountEntryKind::PurchaseReturnCredit.is_manual());
        assert!(!SupplierAccountEntryKind::CreditRefund.is_manual());
        assert!(SupplierAccountEntryKind::AdjustmentIn.is_manual());
        assert!(SupplierAccountEntryKind::AdjustmentOut.is_manual());
    }

    #[test]
    fn entry_kinds_round_trip_wire_names() {
        for kind in [
            SupplierAccountEntryKind::PurchaseCharge,
            SupplierAccountEntryKind::Payment,
            SupplierAccountEntryKind::PurchaseReturn,
            SupplierAccountEntryKind::PurchaseReturnCredit,
            SupplierAccountEntryKind::CreditRefund,
            SupplierAccountEntryKind::AdjustmentIn,
            SupplierAccountEntryKind::AdjustmentOut,
        ] {
            assert_eq!(kind.as_str().parse::<SupplierAccountEntryKind>(), Ok(kind));
        }
    }

    #[test]
    fn position_invariant_checks_identity_and_floor() {
        let valid = SupplierAccountPosition {
            balance: dec!(60.00),
            unapplied_credit: dec!(40.00),
            outstanding_total: dec!(100.00),
            adjustments: dec!(0.00),
        };
        assert!(valid.is_consistent());

        let negative_credit = SupplierAccountPosition {
            balance: dec!(130.00),
            unapplied_credit: dec!(-30.00),
            outstanding_total: dec!(100.00),
            adjustments: dec!(0.00),
        };
        assert!(!negative_credit.is_consistent());
    }
}
