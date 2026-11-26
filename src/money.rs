//! Shared Postgres numeric column ceilings and checked `Decimal` arithmetic.
//!
//! Every monetary and quantity column in Postgres uses one of three precisions:
//! - `numeric(12, 2)`: unit prices and unit costs ([`MAX_UNIT_PRICE`])
//! - `numeric(12, 3)`: quantities and minimum stock ([`MAX_QUANTITY`])
//! - `numeric(14, 2)`: totals, payments, credit limits, and ledger entries ([`MAX_AMOUNT`])
//!
//! Validating inputs and intermediate sums against these bounds with checked
//! arithmetic turns `rust_decimal` overflow panics and Postgres SQLSTATE `22003`
//! (`numeric_value_out_of_range`) errors into `422 Unprocessable Entity`
//! responses.

use rust_decimal::Decimal;

const fn from_u64_scale(n: u64, scale: u32) -> Decimal {
    Decimal::from_parts(n as u32, (n >> 32) as u32, 0, false, scale)
}

/// Maximum value representable in `numeric(12, 2)` (10 integer digits + 2 decimals).
pub const MAX_UNIT_PRICE: Decimal = from_u64_scale(999_999_999_999, 2);

/// Maximum value representable in `numeric(12, 3)` (9 integer digits + 3 decimals).
pub const MAX_QUANTITY: Decimal = from_u64_scale(999_999_999_999, 3);

/// Maximum value representable in `numeric(14, 2)` (12 integer digits + 2 decimals).
pub const MAX_AMOUNT: Decimal = from_u64_scale(99_999_999_999_999, 2);

/// Add two quantities without panicking, ensuring the sum fits in `numeric(12, 3)`.
pub fn checked_add_quantity(a: Decimal, b: Decimal) -> Option<Decimal> {
    a.checked_add(b)
        .filter(|sum| *sum <= MAX_QUANTITY && *sum >= -MAX_QUANTITY)
}

/// Add two monetary amounts without panicking, ensuring the sum fits in `numeric(14, 2)`.
pub fn checked_add_amount(a: Decimal, b: Decimal) -> Option<Decimal> {
    a.checked_add(b)
        .filter(|sum| *sum <= MAX_AMOUNT && *sum >= -MAX_AMOUNT)
}

/// Compute `SUM(quantity * unit_price)` rounded to 2 decimal places using checked
/// arithmetic, returning `None` on overflow or if the rounded total exceeds
/// `numeric(14, 2)`.
pub fn checked_document_total(
    lines: impl IntoIterator<Item = (Decimal, Decimal)>,
) -> Option<Decimal> {
    let mut sum = Decimal::ZERO;
    for (quantity, unit_price) in lines {
        let subtotal = quantity.checked_mul(unit_price)?;
        sum = sum.checked_add(subtotal)?;
    }
    let rounded = sum.round_dp(2);
    (rounded <= MAX_AMOUNT && rounded >= -MAX_AMOUNT).then_some(rounded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn column_ceilings_match_postgres_numeric_precisions() {
        assert_eq!(MAX_UNIT_PRICE, Decimal::new(999_999_999_999, 2));
        assert_eq!(MAX_QUANTITY, Decimal::new(999_999_999_999, 3));
        assert_eq!(
            MAX_AMOUNT,
            Decimal::from_i128_with_scale(99_999_999_999_999, 2)
        );
    }

    #[test]
    fn checked_add_quantity_rejects_ceiling_and_decimal_overflow() {
        assert_eq!(
            checked_add_quantity(dec!(500_000_000), dec!(499_999_999.999)),
            Some(MAX_QUANTITY)
        );
        assert_eq!(
            checked_add_quantity(dec!(600_000_000), dec!(600_000_000)),
            None
        );
        assert_eq!(checked_add_quantity(Decimal::MAX, Decimal::ONE), None);
    }

    #[test]
    fn checked_add_amount_rejects_ceiling_and_decimal_overflow() {
        assert_eq!(
            checked_add_amount(dec!(999_999_999_999.00), dec!(0.99)),
            Some(MAX_AMOUNT)
        );
        assert_eq!(checked_add_amount(MAX_AMOUNT, dec!(0.01)), None);
        assert_eq!(checked_add_amount(Decimal::MAX, Decimal::MAX), None);
    }

    #[test]
    fn checked_document_total_enforces_rounding_boundary_and_overflow() {
        // 999_999_999_999.994 rounds down to MAX_AMOUNT in both Rust and Postgres.
        assert_eq!(
            checked_document_total([
                (dec!(1_000_000), dec!(999_999.99)),
                (dec!(1), dec!(9_999.99)),
                (dec!(0.004), dec!(1.00)),
            ]),
            Some(MAX_AMOUNT)
        );

        // 999_999_999_999.995 rounds up to 1_000_000_000_000.00 (> numeric(14,2))
        // in both banker's rounding (hundredths digit 9 is odd) and half-away-from-zero.
        assert_eq!(
            checked_document_total([
                (dec!(1_000_000), dec!(999_999.99)),
                (dec!(1), dec!(9_999.99)),
                (dec!(0.005), dec!(1.00)),
            ]),
            None
        );

        // Valid numeric(12,3) * valid numeric(12,2) exceeding numeric(14,2).
        assert_eq!(
            checked_document_total([(dec!(200_000), dec!(10_000_000.00))]),
            None
        );

        // Decimal::MAX multiplication overflows without panicking.
        assert_eq!(checked_document_total([(Decimal::MAX, dec!(2))]), None);
    }
}
