//! Sales domain.
//!
//! A sale is a **persisted draft** until checkout. The cart lives in the
//! database, not the browser, so it survives a refresh, network outage, and a
//! cashier leaving to fetch the product.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "sale_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SaleStatus {
    Draft,
    Completed,
    Cancelled,
}

impl SaleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SaleStatus::Draft => "draft",
            SaleStatus::Completed => "completed",
            SaleStatus::Cancelled => "cancelled",
        }
    }

    /// Only a draft is editable. A completed sale is a fact, and a fact is not
    /// corrected: a credit note is issued (not yet implemented, which is why
    /// `cancelled` applies only to drafts).
    pub fn is_editable(self) -> bool {
        matches!(self, SaleStatus::Draft)
    }
}

impl fmt::Display for SaleStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SaleStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "draft" | "borrador" => Ok(SaleStatus::Draft),
            "completed" | "completada" => Ok(SaleStatus::Completed),
            "cancelled" | "cancelada" => Ok(SaleStatus::Cancelled),
            other => Err(format!(
                "unknown status: {other:?} (valid: draft, completed, cancelled)"
            )),
        }
    }
}

/// Item in a sale. The price comes from the product when added and is frozen:
/// if the product price changes during checkout, the total does not jump.
#[derive(Debug, Clone)]
pub struct SaleItem {
    pub id: Uuid,
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
}

impl SaleItem {
    pub fn subtotal(&self) -> Decimal {
        self.quantity * self.unit_price
    }
}

#[derive(Debug, Clone)]
pub struct Sale {
    pub id: Uuid,
    pub status: SaleStatus,
    pub items: Vec<SaleItem>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Sale {
    /// Server-calculated total. The client may show its own total, but this is
    /// the one charged.
    pub fn total(&self) -> Decimal {
        self.items.iter().map(SaleItem::subtotal).sum()
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn item(qty: Decimal, price: Decimal) -> SaleItem {
        SaleItem {
            id: Uuid::now_v7(),
            product_id: Uuid::now_v7(),
            product_name: "x".into(),
            quantity: qty,
            unit_price: price,
        }
    }

    #[test]
    fn total_sums_subtotals_without_float_error() {
        let sale = Sale {
            id: Uuid::now_v7(),
            status: SaleStatus::Draft,
            items: vec![
                item(dec!(2), dec!(2800.00)),
                item(dec!(1.5), dec!(1200.33)),
                item(dec!(3), dec!(0.10)),
            ],
            created_by: Uuid::now_v7(),
            created_at: Utc::now(),
            completed_at: None,
        };

        // 5600.00 + 1800.495 + 0.30 = 7400.795
        // DB `numeric` is (12,3) for quantity and (12,2) for price: the
        // product can have 3 decimals and is rounded to 2 when persisting total.
        assert_eq!(sale.total(), dec!(7400.795));
    }

    #[test]
    fn empty_sale_totals_zero() {
        let sale = Sale {
            id: Uuid::now_v7(),
            status: SaleStatus::Draft,
            items: vec![],
            created_by: Uuid::now_v7(),
            created_at: Utc::now(),
            completed_at: None,
        };
        assert_eq!(sale.total(), Decimal::ZERO);
        assert_eq!(sale.item_count(), 0);
    }

    #[test]
    fn only_drafts_are_editable() {
        assert!(SaleStatus::Draft.is_editable());
        assert!(!SaleStatus::Completed.is_editable());
        assert!(!SaleStatus::Cancelled.is_editable());
    }

    #[test]
    fn status_parses_expected_spellings() {
        assert_eq!(SaleStatus::from_str("borrador").unwrap(), SaleStatus::Draft);
        assert_eq!(
            SaleStatus::from_str("COMPLETED").unwrap(),
            SaleStatus::Completed
        );
        assert!(SaleStatus::from_str("abierta").is_err());
    }
}
