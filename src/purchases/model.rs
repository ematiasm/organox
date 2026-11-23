//! Purchase types.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "purchase_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum PurchaseStatus {
    Draft,
    Completed,
    Cancelled,
}

impl PurchaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PurchaseStatus::Draft => "draft",
            PurchaseStatus::Completed => "completed",
            PurchaseStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_editable(self) -> bool {
        matches!(self, PurchaseStatus::Draft)
    }
}

impl fmt::Display for PurchaseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PurchaseStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "draft" => Ok(PurchaseStatus::Draft),
            "completed" => Ok(PurchaseStatus::Completed),
            "cancelled" => Ok(PurchaseStatus::Cancelled),
            other => Err(format!(
                "unknown status: {other:?} (valid: draft, completed, cancelled)"
            )),
        }
    }
}

/// One line item in a purchase.
#[derive(Debug, Clone)]
pub struct PurchaseItem {
    pub id: Uuid,
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_cost: Decimal,
}

impl PurchaseItem {
    pub fn subtotal(&self) -> Decimal {
        (self.quantity * self.unit_cost).round_dp(2)
    }
}

/// One cash outflow leg when completing a purchase.
#[derive(Debug, Clone)]
pub struct PurchasePaymentInput {
    pub payment_method_id: Uuid,
    pub amount: Decimal,
}

#[derive(Debug, Clone)]
pub struct Purchase {
    pub id: Uuid,
    pub status: PurchaseStatus,
    pub items: Vec<PurchaseItem>,
    /// Frozen total for completed purchases; `None` while draft.
    pub frozen_total: Option<Decimal>,
    pub supplier_id: Option<Uuid>,
    /// Live supplier name while draft; frozen snapshot once completed.
    pub supplier_name: Option<String>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Purchase {
    /// Server-calculated total. Completed purchases return their frozen value.
    pub fn total(&self) -> Decimal {
        self.frozen_total.unwrap_or_else(|| {
            self.items
                .iter()
                .map(|item| item.quantity * item.unit_cost)
                .sum::<Decimal>()
                .round_dp(2)
        })
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn item(qty: Decimal, cost: Decimal) -> PurchaseItem {
        PurchaseItem {
            id: Uuid::now_v7(),
            product_id: Uuid::now_v7(),
            product_name: "Flour".into(),
            quantity: qty,
            unit_cost: cost,
        }
    }

    #[test]
    fn total_sums_subtotals_precisely() {
        let purchase = Purchase {
            id: Uuid::now_v7(),
            status: PurchaseStatus::Draft,
            items: vec![item(dec!(2.5), dec!(120.10)), item(dec!(3), dec!(50.00))],
            frozen_total: None,
            supplier_id: None,
            supplier_name: None,
            note: None,
            created_by: Uuid::now_v7(),
            created_at: Utc::now(),
            completed_at: None,
        };
        assert_eq!(purchase.total(), dec!(450.25));
    }
}
