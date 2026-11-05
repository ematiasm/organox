//! Validación de entradas del catálogo.
//!
//! Las reglas que se validan acá son las que dan un mensaje útil al usuario.
//! Las que no se pueden confiar al código (signo, append-only, servicio sin
//! stock) se imponen además en la DB: la validación de entrada no es la última
//! línea de defensa.

use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("el nombre no puede estar vacío")]
    NameBlank,

    #[error("el nombre supera los {max} caracteres")]
    NameTooLong { max: usize },

    #[error("el SKU supera los {max} caracteres")]
    SkuTooLong { max: usize },

    #[error("el precio no puede ser negativo")]
    PriceNegative,

    #[error("el precio tiene más de 2 decimales")]
    PricePrecision,

    #[error("el stock mínimo no puede ser negativo")]
    MinStockNegative,

    #[error("la cantidad debe ser mayor que cero")]
    QuantityNotPositive,

    #[error("la cantidad tiene más de 3 decimales")]
    QuantityPrecision,

    #[error("el costo unitario no puede ser negativo")]
    UnitCostNegative,

    #[error("el costo unitario tiene más de 2 decimales")]
    UnitCostPrecision,

    #[error("un servicio no lleva stock")]
    ServiceHasNoStock,

    #[error("la cantidad se canceló y no quedó stock suficiente")]
    InsufficientStock,

    #[error("no se encontró el producto")]
    ProductNotFound,
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_SKU_LEN: usize = 64;
pub const MAX_NOTE_LEN: usize = 500;

/// Las cantidades se guardan en `numeric(12,3)`. Validar la escala en el borde
/// evita que Postgres redondee silenciosamente un `1.0004` a `1.000`.
pub const QUANTITY_SCALE: u32 = 3;
/// El dinero en `numeric(12,2)`.
pub const MONEY_SCALE: u32 = 2;

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

/// El SKU vacío se convierte en `None`: "" y NULL no deben ser dos formas de
/// decir "sin SKU", o el índice único parcial los trata distinto.
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
    Ok(Some(min))
}

pub fn validate_quantity(raw: Decimal) -> Result<Decimal, ValidationError> {
    if raw <= Decimal::ZERO {
        return Err(ValidationError::QuantityNotPositive);
    }
    if raw.normalize().scale() > QUANTITY_SCALE {
        return Err(ValidationError::QuantityPrecision);
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
    Ok(Some(cost))
}

pub fn validate_note(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // Se trunca en vez de rechazar: una nota larga no es motivo para perder el
    // movimiento que se está registrando.
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
        // 10.999 no se redondea en silencio a 11.00: se rechaza.
        assert_eq!(
            validate_price(Some(dec!(10.999))).unwrap_err(),
            ValidationError::PricePrecision
        );
        // Los ceros a la derecha no cuentan como precisión.
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
}
