//! Catalog input validation.
//!
//! Rules validated here provide useful messages to the user. Those that cannot
//! be entrusted to code (sign, append-only, services without stock) are also
//! enforced in the DB: input validation is not the last line of defense.

use rust_decimal::Decimal;

use super::model::MovementReason;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("the name cannot be blank")]
    NameBlank,

    #[error("the name exceeds {max} characters")]
    NameTooLong { max: usize },

    #[error("the SKU exceeds {max} characters")]
    SkuTooLong { max: usize },

    #[error("the price cannot be negative")]
    PriceNegative,

    #[error("the price has more than 2 decimal places")]
    PricePrecision,

    #[error("the price cannot exceed {max}")]
    PriceTooLarge { max: Decimal },

    #[error("minimum stock cannot be negative")]
    MinStockNegative,

    #[error("minimum stock cannot exceed {max}")]
    MinStockTooLarge { max: Decimal },

    #[error("the quantity must be greater than zero")]
    QuantityNotPositive,

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("the quantity cannot exceed {max}")]
    QuantityTooLarge { max: Decimal },

    #[error("the unit cost cannot be negative")]
    UnitCostNegative,

    #[error("the unit cost has more than 2 decimal places")]
    UnitCostPrecision,

    #[error("the unit cost cannot exceed {max}")]
    UnitCostTooLarge { max: Decimal },

    #[error("a service does not track stock")]
    ServiceHasNoStock,

    #[error("{}", non_manual_reason_detail(*reason))]
    NonManualReason { reason: MovementReason },

    #[error("the quantity was cancelled and insufficient stock remained")]
    InsufficientStock,

    #[error("the product was not found")]
    ProductNotFound,
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_SKU_LEN: usize = 64;
pub const MAX_NOTE_LEN: usize = 500;

/// Quantities are stored in `numeric(12,3)`. Validating scale at the boundary
/// prevents Postgres from silently rounding `1.0004` to `1.000`.
pub const QUANTITY_SCALE: u32 = 3;
/// Money uses `numeric(12,2)`.
pub const MONEY_SCALE: u32 = 2;

fn non_manual_reason_detail(reason: MovementReason) -> String {
    match reason {
        MovementReason::Return => {
            "return stock movements can only be written through a credit note".into()
        }
        MovementReason::PurchaseReturn => {
            "purchase_return stock movements can only be written through a purchase return".into()
        }
        other => format!("{other} stock movements cannot be recorded manually"),
    }
}

pub fn validate_name(raw: &str) -> Result<String, ValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::NameBlank);
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(ValidationError::NameTooLong { max: MAX_NAME_LEN });
    }
    Ok(trimmed.to_string())
}

/// An empty SKU becomes `None`: "" and NULL must not be two ways of saying
/// "no SKU", or the partial unique index treats them differently.
pub fn validate_sku(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_SKU_LEN {
        return Err(ValidationError::SkuTooLong { max: MAX_SKU_LEN });
    }
    Ok(Some(trimmed.to_string()))
}

pub fn validate_price(raw: Option<Decimal>) -> Result<Option<Decimal>, ValidationError> {
    let Some(price) = raw else { return Ok(None) };
    if price.is_sign_negative() {
        return Err(ValidationError::PriceNegative);
    }
    if price.normalize().scale() > MONEY_SCALE {
        return Err(ValidationError::PricePrecision);
    }
    if price > crate::money::MAX_UNIT_PRICE {
        return Err(ValidationError::PriceTooLarge {
            max: crate::money::MAX_UNIT_PRICE,
        });
    }
    Ok(Some(price))
}

pub fn validate_min_stock(raw: Option<Decimal>) -> Result<Option<Decimal>, ValidationError> {
    let Some(min) = raw else { return Ok(None) };
    if min.is_sign_negative() {
        return Err(ValidationError::MinStockNegative);
    }
    if min.normalize().scale() > QUANTITY_SCALE {
        return Err(ValidationError::QuantityPrecision);
    }
    if min > crate::money::MAX_QUANTITY {
        return Err(ValidationError::MinStockTooLarge {
            max: crate::money::MAX_QUANTITY,
        });
    }
    Ok(Some(min))
}

pub fn validate_quantity(raw: Decimal) -> Result<Decimal, ValidationError> {
    if raw <= Decimal::ZERO {
        return Err(ValidationError::QuantityNotPositive);
    }
    if raw.normalize().scale() > QUANTITY_SCALE {
        return Err(ValidationError::QuantityPrecision);
    }
    if raw > crate::money::MAX_QUANTITY {
        return Err(ValidationError::QuantityTooLarge {
            max: crate::money::MAX_QUANTITY,
        });
    }
    Ok(raw)
}

pub fn validate_unit_cost(raw: Option<Decimal>) -> Result<Option<Decimal>, ValidationError> {
    let Some(cost) = raw else { return Ok(None) };
    if cost.is_sign_negative() {
        return Err(ValidationError::UnitCostNegative);
    }
    if cost.normalize().scale() > MONEY_SCALE {
        return Err(ValidationError::UnitCostPrecision);
    }
    if cost > crate::money::MAX_UNIT_PRICE {
        return Err(ValidationError::UnitCostTooLarge {
            max: crate::money::MAX_UNIT_PRICE,
        });
    }
    Ok(Some(cost))
}

pub fn validate_note(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // Truncate rather than reject: a long note is no reason to lose the
    // movement being recorded.
    let truncated: String = trimmed.chars().take(MAX_NOTE_LEN).collect();
    Ok(Some(truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn name_is_trimmed_and_blank_rejected() {
        assert_eq!(validate_name("  Harina 000  ").unwrap(), "Harina 000");
        assert_eq!(
            validate_name("   ").unwrap_err(),
            ValidationError::NameBlank
        );
        assert_eq!(validate_name("").unwrap_err(), ValidationError::NameBlank);
    }

    #[test]
    fn empty_sku_becomes_none() {
        assert_eq!(validate_sku(Some("   ")).unwrap(), None);
        assert_eq!(validate_sku(None).unwrap(), None);
        assert_eq!(validate_sku(Some(" HAR-1 ")).unwrap(), Some("HAR-1".into()));
    }

    #[test]
    fn price_rejects_negative_and_excess_precision() {
        assert_eq!(
            validate_price(Some(dec!(1500.50))).unwrap(),
            Some(dec!(1500.50))
        );
        assert_eq!(
            validate_price(Some(dec!(-1))).unwrap_err(),
            ValidationError::PriceNegative
        );
        // 10.999 is not silently rounded to 11.00: it is rejected.
        assert_eq!(
            validate_price(Some(dec!(10.999))).unwrap_err(),
            ValidationError::PricePrecision
        );
        // Trailing zeros do not count as precision.
        assert!(validate_price(Some(dec!(10.000))).is_ok());
    }

    #[test]
    fn quantity_must_be_positive_and_within_scale() {
        assert_eq!(validate_quantity(dec!(3.5)).unwrap(), dec!(3.5));
        assert_eq!(
            validate_quantity(dec!(0)).unwrap_err(),
            ValidationError::QuantityNotPositive
        );
        assert_eq!(
            validate_quantity(dec!(-1)).unwrap_err(),
            ValidationError::QuantityNotPositive
        );
        assert_eq!(
            validate_quantity(dec!(1.0004)).unwrap_err(),
            ValidationError::QuantityPrecision
        );
    }

    #[test]
    fn long_note_is_truncated_not_rejected() {
        let long = "x".repeat(MAX_NOTE_LEN + 50);
        let note = validate_note(Some(&long)).unwrap().unwrap();
        assert_eq!(note.chars().count(), MAX_NOTE_LEN);
    }

    #[test]
    fn numeric_column_ceilings_are_enforced() {
        // numeric(12,2): max 9_999_999_999.99
        assert!(validate_price(Some(dec!(9_999_999_999.99))).is_ok());
        assert!(validate_price(Some(dec!(10_000_000_000.00))).is_err());
        assert!(validate_unit_cost(Some(dec!(9_999_999_999.99))).is_ok());
        assert!(validate_unit_cost(Some(dec!(10_000_000_000.00))).is_err());

        // numeric(12,3): max 999_999_999.999
        assert!(validate_min_stock(Some(dec!(999_999_999.999))).is_ok());
        assert!(validate_min_stock(Some(dec!(1_000_000_000))).is_err());
        assert!(validate_quantity(dec!(999_999_999.999)).is_ok());
        assert!(validate_quantity(dec!(1_000_000_000)).is_err());
    }

    #[test]
    fn non_manual_reason_error_names_the_owning_document() {
        let customer_return = ValidationError::NonManualReason {
            reason: MovementReason::Return,
        }
        .to_string();
        assert_eq!(
            customer_return,
            "return stock movements can only be written through a credit note"
        );

        let supplier_return = ValidationError::NonManualReason {
            reason: MovementReason::PurchaseReturn,
        }
        .to_string();
        assert_eq!(
            supplier_return,
            "purchase_return stock movements can only be written through a purchase return"
        );
    }
}
