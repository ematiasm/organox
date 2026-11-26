//! Purchase-return domain types and proportional refund splitting.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest accepted reason, in characters after trimming.
pub const MAX_REASON_LEN: usize = 500;

/// Longest accepted idempotency key, in characters after trimming.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

/// How an already-paid surplus (the portion of a return that exceeds the
/// purchase's open payable debt) should be settled.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "purchase_return_surplus_mode", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SurplusMode {
    /// Refund the surplus back into the wallets that paid for the purchase,
    /// proportionally to each tender's remaining refundable capacity.
    #[default]
    WalletRefund,
    /// Leave the surplus as unapplied advance credit on the supplier's account
    /// so future on-account purchases consume it oldest-first (or a later
    /// `POST /suppliers/{id}/credit-refunds` refunds it in cash).
    SupplierCredit,
}

/// Stored purchase-return header.
#[derive(Debug, Clone)]
pub struct PurchaseReturn {
    pub id: Uuid,
    pub purchase_id: Uuid,
    pub total: Decimal,
    pub surplus_mode: SurplusMode,
    pub reason: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One returned line with the frozen `unit_cost` copied from `purchase_items`.
#[derive(Debug, Clone)]
pub struct PurchaseReturnItem {
    pub id: Uuid,
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_cost: Decimal,
}

impl PurchaseReturnItem {
    pub fn subtotal(&self) -> Decimal {
        crate::money::line_subtotal(self.quantity, self.unit_cost)
    }
}

/// One wallet inflow produced when `surplus_mode = wallet_refund`.
#[derive(Debug, Clone)]
pub struct PurchaseReturnRefund {
    pub wallet_id: Uuid,
    pub wallet_name: String,
    pub payment_method_id: Uuid,
    pub payment_method_name: String,
    pub amount: Decimal,
}

/// Complete return document with lines, debt cancelled, supplier credit
/// granted, and wallet refunds.
#[derive(Debug, Clone)]
pub struct PurchaseReturnDetail {
    pub ret: PurchaseReturn,
    pub debt_cancelled: Decimal,
    pub credit_granted: Decimal,
    pub items: Vec<PurchaseReturnItem>,
    pub refunds: Vec<PurchaseReturnRefund>,
}

/// Result of creating a return, distinguishing fresh creates (`201`) from
/// idempotent replays (`200`).
#[derive(Debug, Clone)]
pub struct CreatedPurchaseReturn {
    pub detail: PurchaseReturnDetail,
    pub replayed: bool,
}

impl std::ops::Deref for CreatedPurchaseReturn {
    type Target = PurchaseReturnDetail;

    fn deref(&self) -> &Self::Target {
        &self.detail
    }
}

/// A line of the linked purchase with cumulative returned and remaining
/// returnable quantities.
#[derive(Debug, Clone)]
pub struct ReturnableLine {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_cost: Decimal,
    pub returned_quantity: Decimal,
}

impl ReturnableLine {
    pub fn returnable_quantity(&self) -> Decimal {
        self.quantity - self.returned_quantity
    }
}

/// Caller-supplied line to return. Unit cost is never supplied by the client.
#[derive(Debug, Clone, Copy)]
pub struct ReturnLine {
    pub product_id: Uuid,
    pub quantity: Decimal,
}

/// Split `surplus` proportionally across tenders using each tender's
/// **remaining** refundable capacity (`paid - prior_refunded`).
///
/// Unlike subtracting two cumulative splits, allocating directly against
/// remaining capacities is strictly non-negative for every number of tenders
/// (no Alabama paradox on 3+ tenders) and composes cleanly when some returns
/// on the same purchase use `supplier_credit` instead of `wallet_refund`.
pub fn split_remaining_capacity(surplus: Decimal, remaining_caps: &[Decimal]) -> Vec<Decimal> {
    crate::credit_notes::model::split_cash_refund(surplus, remaining_caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn remaining_capacity_split_handles_three_tenders_across_multiple_returns_without_paradox() {
        // Classic 3-tender sequential partial refunds: allocating against
        // remaining capacities always produces non-negative shares summing
        // exactly to each return's surplus and never exceeding any tender cap.
        let mut remaining = vec![dec!(10.00), dec!(30.00), dec!(60.00)];
        let returns = [dec!(1.00), dec!(13.33), dec!(25.67), dec!(60.00)];

        for surplus in returns {
            let shares = split_remaining_capacity(surplus, &remaining);
            assert_eq!(shares.iter().copied().sum::<Decimal>(), surplus);
            for (rem, share) in remaining.iter_mut().zip(shares) {
                assert!(share >= Decimal::ZERO);
                assert!(share <= *rem);
                *rem -= share;
            }
        }
        assert_eq!(remaining, vec![dec!(0.00), dec!(0.00), dec!(0.00)]);
    }

    #[test]
    fn line_returnable_quantity_and_subtotal_use_frozen_cost() {
        let line = ReturnableLine {
            product_id: Uuid::now_v7(),
            product_name: "Flour".into(),
            quantity: dec!(10),
            unit_cost: dec!(125.50),
            returned_quantity: dec!(3.5),
        };
        assert_eq!(line.returnable_quantity(), dec!(6.5));

        let item = PurchaseReturnItem {
            id: Uuid::now_v7(),
            product_id: line.product_id,
            product_name: line.product_name,
            quantity: dec!(2.5),
            unit_cost: dec!(125.50),
        };
        assert_eq!(item.subtotal(), dec!(313.75));
    }
}
