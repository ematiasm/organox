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
    Expense,
    AdjustmentIn,
    AdjustmentOut,
}

impl WalletMovementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WalletMovementKind::SalePayment => "sale_payment",
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
            WalletMovementKind::SalePayment | WalletMovementKind::AdjustmentIn => 1,
            WalletMovementKind::Expense | WalletMovementKind::AdjustmentOut => -1,
        }
    }

    pub fn signed_effect(self, amount: Decimal) -> Decimal {
        amount * Decimal::from(self.sign())
    }

    /// Kinds an operator records by hand. A sale payment is only ever produced
    /// by charging a sale, never typed into a form.
    pub fn is_manual(self) -> bool {
        !matches!(self, WalletMovementKind::SalePayment)
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
            "expense" | "gasto" => Ok(WalletMovementKind::Expense),
            "adjustment_in" | "ajuste_entrada" => Ok(WalletMovementKind::AdjustmentIn),
            "adjustment_out" | "ajuste_salida" => Ok(WalletMovementKind::AdjustmentOut),
            other => Err(format!(
                "unknown kind: {other:?} (valid: sale_payment, expense, adjustment_in, adjustment_out)"
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
    fn sale_payments_and_adjustment_in_add_money() {
        assert_eq!(
            WalletMovementKind::SalePayment.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
        assert_eq!(
            WalletMovementKind::AdjustmentIn.signed_effect(dec!(100.00)),
            dec!(100.00)
        );
    }

    #[test]
    fn expenses_and_adjustment_out_remove_money() {
        assert_eq!(
            WalletMovementKind::Expense.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
        assert_eq!(
            WalletMovementKind::AdjustmentOut.signed_effect(dec!(100.00)),
            dec!(-100.00)
        );
    }

    #[test]
    fn only_a_sale_payment_is_not_manual() {
        assert!(!WalletMovementKind::SalePayment.is_manual());
        assert!(WalletMovementKind::Expense.is_manual());
        assert!(WalletMovementKind::AdjustmentIn.is_manual());
        assert!(WalletMovementKind::AdjustmentOut.is_manual());
    }

    #[test]
    fn kinds_round_trip_through_their_wire_value() {
        for kind in [
            WalletMovementKind::SalePayment,
            WalletMovementKind::Expense,
            WalletMovementKind::AdjustmentIn,
            WalletMovementKind::AdjustmentOut,
        ] {
            assert_eq!(kind.as_str().parse::<WalletMovementKind>(), Ok(kind));
        }
    }
}
