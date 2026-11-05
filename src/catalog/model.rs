//! Catalog types.
//!
//! Layer decision: stock **is not a field**. It is the signed sum of movements.
//! `Product` has no `stock`, and `ProductWithStock` adds it as a query result,
//! never as something stored and editable.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// A catalog item may or may not track stock.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "product_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ProductKind {
    Product,
    Service,
}

impl ProductKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProductKind::Product => "product",
            ProductKind::Service => "service",
        }
    }

    /// Only products track stock. This rule is consulted by the service before
    /// accepting a movement, and also enforced by the DB with
    /// `products_service_has_no_min_stock`.
    pub fn tracks_stock(self) -> bool {
        matches!(self, ProductKind::Product)
    }
}

impl fmt::Display for ProductKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProductKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "product" | "producto" => Ok(ProductKind::Product),
            "service" | "servicio" => Ok(ProductKind::Service),
            other => Err(format!("unknown kind: {other:?} (valid: product, service)")),
        }
    }
}

/// Why stock was moved.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema,
)]
#[sqlx(type_name = "movement_reason", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MovementReason {
    Purchase,
    Sale,
    Adjustment,
    Loss,
}

impl MovementReason {
    pub fn as_str(self) -> &'static str {
        match self {
            MovementReason::Purchase => "purchase",
            MovementReason::Sale => "sale",
            MovementReason::Adjustment => "adjustment",
            MovementReason::Loss => "loss",
        }
    }

    /// The sign the reason applies to stock. Quantity is always stored positive;
    /// only this function decides whether to add or subtract. Keeping it here
    /// (rather than in scattered SQL) avoids the classic bug of a sale adding
    /// stock because someone forgot the `-`.
    pub fn sign(self) -> i8 {
        match self {
            MovementReason::Purchase | MovementReason::Adjustment => 1,
            MovementReason::Sale | MovementReason::Loss => -1,
        }
    }

    /// This movement's signed contribution to stock.
    pub fn signed_effect(self, quantity: Decimal) -> Decimal {
        quantity * Decimal::from(self.sign())
    }
}

impl fmt::Display for MovementReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MovementReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "purchase" | "compra" => Ok(MovementReason::Purchase),
            "sale" | "venta" => Ok(MovementReason::Sale),
            "adjustment" | "ajuste" => Ok(MovementReason::Adjustment),
            "loss" | "perdida" | "p\u{e9}rdida" => Ok(MovementReason::Loss),
            other => Err(format!(
                "unknown reason: {other:?} (valid: purchase, sale, adjustment, loss)"
            )),
        }
    }
}

/// Catalog item as stored in the DB. No stock field: stock is calculated.
#[derive(Debug, Clone)]
pub struct Product {
    pub id: Uuid,
    pub kind: ProductKind,
    pub sku: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub price: Option<Decimal>,
    pub min_stock: Option<Decimal>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Product + calculated stock. `stock` is `None` for services: it is not zero,
/// it is "not applicable". That distinction matters in the API.
#[derive(Debug, Clone)]
pub struct ProductWithStock {
    pub product: Product,
    pub stock: Option<Decimal>,
}

/// Whether the product is below the minimum.
///
/// A product without `min_stock` configured is never low: there is no threshold
/// to violate. Neither is a service, because it does not track stock.
pub fn is_low_stock(product: &Product, stock: Option<Decimal>) -> Option<bool> {
    let (min_stock, stock) = (product.min_stock?, stock?);
    Some(stock <= min_stock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn only_products_track_stock() {
        assert!(ProductKind::Product.tracks_stock());
        assert!(!ProductKind::Service.tracks_stock());
    }

    #[test]
    fn entry_reasons_add_and_exit_reasons_subtract() {
        assert_eq!(MovementReason::Purchase.sign(), 1);
        assert_eq!(MovementReason::Adjustment.sign(), 1);
        assert_eq!(MovementReason::Sale.sign(), -1);
        assert_eq!(MovementReason::Loss.sign(), -1);
    }

    #[test]
    fn signed_effect_never_changes_the_stored_quantity() {
        // Stored quantity is always positive; the sign is derived.
        let qty = dec!(3.5);
        assert_eq!(MovementReason::Sale.signed_effect(qty), dec!(-3.5));
        assert_eq!(MovementReason::Purchase.signed_effect(qty), dec!(3.5));
    }

    #[test]
    fn enums_parse_from_expected_spellings() {
        assert_eq!(
            ProductKind::from_str("producto").unwrap(),
            ProductKind::Product
        );
        assert_eq!(
            ProductKind::from_str("SERVICE").unwrap(),
            ProductKind::Service
        );
        assert!(ProductKind::from_str("insumo").is_err());

        assert_eq!(
            MovementReason::from_str("venta").unwrap(),
            MovementReason::Sale
        );
        assert_eq!(
            MovementReason::from_str("p\u{e9}rdida").unwrap(),
            MovementReason::Loss
        );
        assert!(MovementReason::from_str("merma").is_err());
    }

    #[test]
    fn low_stock_requires_threshold_and_applicable_stock() {
        let mut product = Product {
            id: Uuid::now_v7(),
            kind: ProductKind::Product,
            sku: None,
            name: "Harina".into(),
            description: None,
            price: Some(dec!(100)),
            min_stock: None,
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // No configured threshold: no alert is possible.
        assert_eq!(is_low_stock(&product, Some(dec!(0))), None);

        product.min_stock = Some(dec!(5));
        assert_eq!(is_low_stock(&product, Some(dec!(3))), Some(true));
        assert_eq!(is_low_stock(&product, Some(dec!(5))), Some(true));
        assert_eq!(is_low_stock(&product, Some(dec!(6))), Some(false));
        // Service: stock does not apply, never alerts.
        assert_eq!(is_low_stock(&product, None), None);
    }
}
