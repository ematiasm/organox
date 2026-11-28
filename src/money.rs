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

use rust_decimal::{Decimal, RoundingStrategy};

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

/// Round a monetary value to 2 decimal places using half-away-from-zero,
/// matching Postgres `ROUND(..., 2)` and `numeric(..., 2)` casts.
///
/// Normalizes `-0.00` to `0.00` so formatting never emits a negative zero.
pub fn round_money(value: Decimal) -> Decimal {
    let rounded = value.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    if rounded.is_zero() {
        Decimal::ZERO
    } else {
        rounded
    }
}

/// Format a monetary value with exactly 2 decimal places using [`round_money`]
/// so `Display` never falls back to banker's rounding on values with >2 decimals.
pub fn format_money(value: Decimal) -> String {
    format!("{:.2}", round_money(value))
}

/// Quantity without extra zeros: `"12.5"`, not `"12.500"`.
///
/// The column is `numeric(12,3)` and `SUM` returns the full scale; displaying
/// the value raw would leak a storage detail into the HTTP contract.
pub fn format_quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Compute a single document line's rounded subtotal (`ROUND(quantity * unit_price, 2)`
/// half-away-from-zero).
pub fn line_subtotal(quantity: Decimal, unit_price: Decimal) -> Decimal {
    round_money(quantity * unit_price)
}

/// Checked version of [`line_subtotal`], returning `None` on overflow or if
/// the rounded subtotal exceeds `numeric(14, 2)`.
pub fn checked_line_subtotal(quantity: Decimal, unit_price: Decimal) -> Option<Decimal> {
    let rounded = round_money(quantity.checked_mul(unit_price)?);
    (rounded <= MAX_AMOUNT && rounded >= -MAX_AMOUNT).then_some(rounded)
}

/// Compute `SUM(ROUND(quantity * unit_price, 2))` using half-away-from-zero
/// per-line rounding and checked arithmetic, returning `None` on overflow or
/// if any line or the document total exceeds `numeric(14, 2)`.
pub fn checked_document_total(
    lines: impl IntoIterator<Item = (Decimal, Decimal)>,
) -> Option<Decimal> {
    let mut sum = Decimal::ZERO;
    for (quantity, unit_price) in lines {
        let subtotal = checked_line_subtotal(quantity, unit_price)?;
        sum = checked_add_amount(sum, subtotal)?;
    }
    Some(sum)
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

        // 0.005 * 1.00 = 0.005 rounds up to 0.01 under half-away-from-zero (banker's
        // rounding would have rounded to 0.00), pushing the total to 1_000_000_000_000.00
        // (> numeric(14,2)).
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

    #[test]
    fn round_and_format_money_use_half_away_from_zero_and_normalize_negative_zero() {
        // Midpoints with even hundredths digit must round away from zero (matching Postgres).
        assert_eq!(round_money(dec!(0.005)), dec!(0.01));
        assert_eq!(round_money(dec!(1.605)), dec!(1.61));
        assert_eq!(round_money(dec!(0.505)), dec!(0.51));
        assert_eq!(round_money(dec!(2.505)), dec!(2.51));
        assert_eq!(round_money(dec!(-1.605)), dec!(-1.61));
        assert_eq!(round_money(dec!(-2.505)), dec!(-2.51));
        assert_eq!(round_money(dec!(-0.005)), dec!(-0.01));
        assert_eq!(round_money(dec!(-0.004)), dec!(0.00));

        assert_eq!(format_money(dec!(1.605)), "1.61");
        assert_eq!(format_money(dec!(0.505)), "0.51");
        assert_eq!(format_money(dec!(-1.605)), "-1.61");
        assert_eq!(format_money(dec!(-0.004)), "0.00");
        assert_eq!(format_money(-dec!(0.00)), "0.00");

        // Empty documents total zero, and extreme line products fail closed.
        assert_eq!(checked_document_total([]), Some(Decimal::ZERO));
        assert_eq!(checked_line_subtotal(MAX_QUANTITY, MAX_UNIT_PRICE), None);

        // Two 0.500 * 1.01 = 0.505 lines round individually to 0.51 and sum to 1.02.
        assert_eq!(line_subtotal(dec!(0.500), dec!(1.01)), dec!(0.51));
        assert_eq!(
            checked_document_total([(dec!(0.500), dec!(1.01)), (dec!(0.500), dec!(1.01))]),
            Some(dec!(1.02))
        );
    }
}
