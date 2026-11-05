//! Tipos del catálogo.
//!
//! Decisión de esta capa: el stock **no es un campo**. Es la suma firmada de los
//! movimientos. `Product` no tiene `stock`, y `ProductWithStock` lo agrega como
//! resultado de una consulta, nunca como algo almacenado y editable.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Un ítem del catálogo puede llevar stock o no.
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

    /// Solo los productos llevan stock. Esta es la regla que el servicio
    /// consulta antes de aceptar un movimiento, y la misma que la DB impone con
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
            other => Err(format!(
                "tipo desconocido: {other:?} (válidos: product, service)"
            )),
        }
    }
}

/// Por qué se movió el stock.
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

    /// El signo que el motivo aplica al stock. La cantidad se guarda siempre
    /// positiva; esta función es la única que decide si suma o resta. Tenerla
    /// acá (y no en SQL disperso) evita el bug clásico de una venta que suma
    /// porque alguien se olvidó el `-`.
    pub fn sign(self) -> i8 {
        match self {
            MovementReason::Purchase | MovementReason::Adjustment => 1,
            MovementReason::Sale | MovementReason::Loss => -1,
        }
    }

    /// Cuánto aporta este movimiento al stock, con signo.
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
            "loss" | "perdida" | "pérdida" => Ok(MovementReason::Loss),
            other => Err(format!(
                "motivo desconocido: {other:?} (válidos: purchase, sale, adjustment, loss)"
            )),
        }
    }
}

/// Ítem del catálogo tal como vive en la DB. Sin stock: el stock se calcula.
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

/// Producto + stock calculado. `stock` es `None` para servicios: no es cero,
/// es "no aplica". Esa distinción importa en la API.
#[derive(Debug, Clone)]
pub struct ProductWithStock {
    pub product: Product,
    pub stock: Option<Decimal>,
}

/// Si el producto está por debajo del mínimo.
///
/// Un producto sin `min_stock` configurado nunca está bajo: no hay umbral que
/// violar. Un servicio tampoco, porque no lleva stock.
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
        // La cantidad guardada es siempre positiva; el signo es derivado.
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

        // Sin umbral configurado: no hay alerta posible.
        assert_eq!(is_low_stock(&product, Some(dec!(0))), None);

        product.min_stock = Some(dec!(5));
        assert_eq!(is_low_stock(&product, Some(dec!(3))), Some(true));
        assert_eq!(is_low_stock(&product, Some(dec!(5))), Some(true));
        assert_eq!(is_low_stock(&product, Some(dec!(6))), Some(false));
        // Servicio: stock no aplica, nunca alerta.
        assert_eq!(is_low_stock(&product, None), None);
    }
}
