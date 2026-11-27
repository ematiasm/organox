//! Payments domain: wallets, payment methods and the money ledger.
//!
//! Layer decision: a wallet has **no** `balance` field. The balance is the signed
//! sum of its movements, exactly as stock is the signed sum of
//! `stock_movements`. A stored balance drifts the first time a transaction
//! forgets to update it, and after that nobody can say which number is true.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Why money moved.
///
/// This is an enum while payment methods are rows, and the asymmetry is the
/// point: a new method is business data the operator creates, but a new kind
/// always arrives with a writer, a validation and a screen — that is, with code.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "wallet_movement_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum WalletMovementKind {
    SalePayment,
    /// Money handed back to a customer for a returned sale. The outflow side of
    /// `sale_payment`, and like it, only ever written by a sale document.
    SaleRefund,
    /// Money collected for a customer's account, written only by the
    /// collection endpoint (`POST /customers/{id}/receipts`) — never by hand
    /// and never by a sale: the allocations of the receipt, not a `sale_id`,
    /// say which sales it paid.
    AccountPayment,
    /// Money handed back to a customer against their credit balance, written
    /// only by the credit-refund endpoint (`POST /customers/{id}/credit-refunds`),
    /// never by hand.
    AccountPaymentRefund,
    /// Money paid out to a supplier when completing a cash purchase, written
    /// only by `POST /purchases/{id}/complete`, never by hand.
    PurchasePayment,
    /// Money returned by a supplier into a wallet for a purchase return,
    /// written only by `POST /purchases/{id}/returns`, never by hand.
    PurchaseRefund,
    /// Money paid out against a supplier's account, written only by
    /// `POST /suppliers/{id}/payments`, never by hand.
    SupplierAccountPayment,
    /// Money received back from a supplier against unapplied advance/credit,
    /// written only by `POST /suppliers/{id}/credit-refunds`, never by hand.
    SupplierAccountRefund,
    Expense,
    AdjustmentIn,
    AdjustmentOut,
}

impl WalletMovementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WalletMovementKind::SalePayment => "sale_payment",
            WalletMovementKind::SaleRefund => "sale_refund",
            WalletMovementKind::AccountPayment => "account_payment",
            WalletMovementKind::AccountPaymentRefund => "account_payment_refund",
            WalletMovementKind::PurchasePayment => "purchase_payment",
            WalletMovementKind::PurchaseRefund => "purchase_refund",
            WalletMovementKind::SupplierAccountPayment => "supplier_account_payment",
            WalletMovementKind::SupplierAccountRefund => "supplier_account_refund",
            WalletMovementKind::Expense => "expense",
            WalletMovementKind::AdjustmentIn => "adjustment_in",
            WalletMovementKind::AdjustmentOut => "adjustment_out",
        }
    }

    /// The sign this kind applies to its wallet. The amount is always stored
    /// positive; only this function decides whether money is added or removed.
    /// Deliberately mirrors `MovementReason::sign`, so there is one place to
    /// look for "does this kind add or subtract".
    pub fn sign(self) -> i8 {
        match self {
            WalletMovementKind::SalePayment
            | WalletMovementKind::AccountPayment
            | WalletMovementKind::PurchaseRefund
            | WalletMovementKind::SupplierAccountRefund
            | WalletMovementKind::AdjustmentIn => 1,
            WalletMovementKind::SaleRefund
            | WalletMovementKind::AccountPaymentRefund
            | WalletMovementKind::PurchasePayment
            | WalletMovementKind::SupplierAccountPayment
            | WalletMovementKind::Expense
            | WalletMovementKind::AdjustmentOut => -1,
        }
    }

    pub fn signed_effect(self, amount: Decimal) -> Decimal {
        amount * Decimal::from(self.sign())
    }

    /// Kinds an operator records by hand.
    pub fn is_manual(self) -> bool {
        self.manual_entry_rejection().is_none()
    }

    /// Why this kind cannot be typed by hand, or `None` when it can.
    ///
    /// One table owns the mapping and [`Self::is_manual`] is derived from it, so
    /// a new kind cannot answer the predicate and the explanation differently.
    /// Each phrase names the endpoint that does record the money; the same list
    /// lives in `docs/payments.md`.
    pub fn manual_entry_rejection(self) -> Option<&'static str> {
        match self {
            WalletMovementKind::SalePayment => {
                Some("a sale payment is recorded by charging a sale, not by hand")
            }
            WalletMovementKind::SaleRefund => {
                Some("a sale refund is recorded by a credit note, not by hand")
            }
            WalletMovementKind::AccountPayment => Some(
                "an account payment is recorded by collecting against a customer's account, not by hand",
            ),
            WalletMovementKind::AccountPaymentRefund => Some(
                "an account payment refund is recorded by refunding customer credit, not by hand",
            ),
            WalletMovementKind::PurchasePayment => {
                Some("a purchase payment is recorded by completing a purchase, not by hand")
            }
            WalletMovementKind::PurchaseRefund => {
                Some("a purchase refund is recorded by a purchase return, not by hand")
            }
            WalletMovementKind::SupplierAccountPayment => Some(
                "a supplier account payment is recorded by paying against a supplier's account, not by hand",
            ),
            WalletMovementKind::SupplierAccountRefund => Some(
                "a supplier account refund is recorded by refunding supplier credit, not by hand",
            ),
            WalletMovementKind::Expense
            | WalletMovementKind::AdjustmentIn
            | WalletMovementKind::AdjustmentOut => None,
        }
    }
}

impl fmt::Display for WalletMovementKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WalletMovementKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "sale_payment" | "cobro" => Ok(WalletMovementKind::SalePayment),
            "sale_refund" => Ok(WalletMovementKind::SaleRefund),
            "account_payment" => Ok(WalletMovementKind::AccountPayment),
            "account_payment_refund" => Ok(WalletMovementKind::AccountPaymentRefund),
            "purchase_payment" => Ok(WalletMovementKind::PurchasePayment),
            "purchase_refund" => Ok(WalletMovementKind::PurchaseRefund),
            "supplier_account_payment" => Ok(WalletMovementKind::SupplierAccountPayment),
            "supplier_account_refund" => Ok(WalletMovementKind::SupplierAccountRefund),
            "expense" | "gasto" => Ok(WalletMovementKind::Expense),
            "adjustment_in" | "ajuste_entrada" => Ok(WalletMovementKind::AdjustmentIn),
            "adjustment_out" | "ajuste_salida" => Ok(WalletMovementKind::AdjustmentOut),
            other => Err(format!(
                "unknown kind: {other:?} (valid: sale_payment, sale_refund, account_payment, account_payment_refund, purchase_payment, purchase_refund, supplier_account_payment, supplier_account_refund, expense, adjustment_in, adjustment_out)"
            )),
        }
    }
}

/// A place money sits: a cash box, a bank account. Data, not code.
#[derive(Debug, Clone)]
pub struct Wallet {
    pub id: Uuid,
    pub name: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A wallet with its calculated balance.
#[derive(Debug, Clone)]
pub struct WalletWithBalance {
    pub wallet: Wallet,
    pub balance: Decimal,
}

/// How a sale is paid. A row, so a new method never needs a migration.
#[derive(Debug, Clone)]
pub struct PaymentMethod {
    pub id: Uuid,
    pub name: String,
    /// The wallet this method pays into **by default**. Never the read path for
    /// past movements: each movement records the wallet it actually hit.
    pub wallet_id: Uuid,
    pub wallet_name: String,
    /// A deactivated wallet accepts no new money, even through an active method.
    pub wallet_is_active: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One line of the money ledger. Append-only.
#[derive(Debug, Clone)]
pub struct WalletMovement {
    pub id: Uuid,
    pub wallet_id: Uuid,
    pub kind: WalletMovementKind,
    /// Always positive. The sign lives in `kind`.
    pub amount: Decimal,
    pub sale_id: Option<Uuid>,
    pub purchase_id: Option<Uuid>,
    pub payment_method_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

impl WalletMovement {
    pub fn signed_amount(&self) -> Decimal {
        self.kind.signed_effect(self.amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn sale_payments_adjustments_and_account_payments_add_money() {
        assert_eq!(
            WalletMovementKind::SalePayment.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
        assert_eq!(
            WalletMovementKind::AdjustmentIn.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
        assert_eq!(
            WalletMovementKind::AccountPayment.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
        assert_eq!(
            WalletMovementKind::PurchaseRefund.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
        assert_eq!(
            WalletMovementKind::SupplierAccountRefund.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
    }

    #[test]
    fn expenses_adjustment_out_and_sale_refunds_remove_money() {
        assert_eq!(
            WalletMovementKind::Expense.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::SaleRefund.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::AccountPaymentRefund.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::PurchasePayment.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::SupplierAccountPayment.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::AdjustmentOut.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
    }

    #[test]
    fn only_the_documented_kinds_are_not_manual() {
        assert!(!WalletMovementKind::SalePayment.is_manual());
        assert!(!WalletMovementKind::SaleRefund.is_manual());
        assert!(!WalletMovementKind::AccountPayment.is_manual());
        assert!(!WalletMovementKind::AccountPaymentRefund.is_manual());
        assert!(!WalletMovementKind::PurchasePayment.is_manual());
        assert!(!WalletMovementKind::PurchaseRefund.is_manual());
        assert!(!WalletMovementKind::SupplierAccountPayment.is_manual());
        assert!(!WalletMovementKind::SupplierAccountRefund.is_manual());
        assert!(WalletMovementKind::Expense.is_manual());
        assert!(WalletMovementKind::AdjustmentIn.is_manual());
        assert!(WalletMovementKind::AdjustmentOut.is_manual());
    }

    #[test]
    fn kinds_round_trip_through_their_wire_value() {
        for kind in [
            WalletMovementKind::SalePayment,
            WalletMovementKind::SaleRefund,
            WalletMovementKind::AccountPayment,
            WalletMovementKind::AccountPaymentRefund,
            WalletMovementKind::PurchasePayment,
            WalletMovementKind::PurchaseRefund,
            WalletMovementKind::SupplierAccountPayment,
            WalletMovementKind::SupplierAccountRefund,
            WalletMovementKind::Expense,
            WalletMovementKind::AdjustmentIn,
            WalletMovementKind::AdjustmentOut,
        ] {
            assert_eq!(kind.as_str().parse::<WalletMovementKind>(), Ok(kind));
        }
    }
}
