//! Credit notes domain: a customer returns part or all of a charged sale.
//!
//! The central decision: the note is its own document and the sale is not
//! edited. `completed` is a fact, so correcting it means recording *what came
//! back*, with its own reason, lines, stock movements and money movements. See
//! `docs/sales.md` and `docs/credit-notes.md`.
//!
//! Nothing here stores a balance. Stock is the signed sum of the stock ledger
//! and a wallet is the signed sum of its movements, exactly as everywhere else
//! in the crate; this module only decides the sign of what it writes.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Longest accepted reason, in characters after trimming.
pub const MAX_REASON_LEN: usize = 200;

/// A credit note as stored.
///
/// The total is the amount computed when the note was issued; it is not
/// recalculated on read, because a later price change must not rewrite what the
/// customer was handed.
#[derive(Debug, Clone)]
pub struct CreditNote {
    pub id: Uuid,
    pub sale_id: Uuid,
    pub reason: String,
    pub total: Decimal,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One returned line. `unit_price` is the price on the sale line, copied when
/// the note is issued, never read from the product again.
#[derive(Debug, Clone)]
pub struct CreditNoteItem {
    pub id: Uuid,
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
}

impl CreditNoteItem {
    pub fn subtotal(&self) -> Decimal {
        self.quantity * self.unit_price
    }
}

/// Where the money went back: one per original payment that received money, so
/// a cash-and-card sale is refunded to both wallets in the same proportion.
#[derive(Debug, Clone)]
pub struct CreditNoteRefund {
    pub wallet_id: Uuid,
    pub wallet_name: String,
    pub payment_method_id: Uuid,
    pub payment_method_name: String,
    pub amount: Decimal,
}

/// The document with its lines and its refunds, which is what the API returns
/// after creating a note and what the sale's notes list is made of.
#[derive(Debug, Clone)]
pub struct CreditNoteDetail {
    pub note: CreditNote,
    pub items: Vec<CreditNoteItem>,
    pub refunds: Vec<CreditNoteRefund>,
}

/// A line of the charged sale as the return screen needs it: what was sold,
/// what has already come back, and what can still come back.
#[derive(Debug, Clone)]
pub struct CreditableLine {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub credited_quantity: Decimal,
}

impl CreditableLine {
    pub fn returnable_quantity(&self) -> Decimal {
        self.quantity - self.credited_quantity
    }
}

/// Split `total` across the original payments, in proportion to each one.
///
/// The result is aligned one-to-one with `payments`, so the caller knows which
/// wallet each share belongs to. Every share is rounded to cents and the last
/// payment absorbs the rounding remainder, so the shares add up to `total`
/// exactly: never a cent more, never a cent less.
///
/// A share of zero stays in place, so the alignment survives, but it is never
/// written to the ledger (`wallet_movements.amount > 0`). The sum invariant is
/// what the caller can rely on; the alignment is what keeps each refund in the
/// wallet that collected the money.
pub fn allocate_refund(total: Decimal, payments: &[Decimal]) -> Vec<Decimal> {
    if payments.is_empty() || total <= Decimal::ZERO {
        return Vec::new();
    }

    let paid: Decimal = payments.iter().copied().sum();
    if paid <= Decimal::ZERO {
        return Vec::new();
    }

    let mut shares = Vec::with_capacity(payments.len());
    let mut allocated = Decimal::ZERO;
    for (index, payment) in payments.iter().enumerate() {
        let remaining = total - allocated;
        let share = if index + 1 == payments.len() {
            // The remainder lands here so the shares sum to `total` exactly.
            remaining
        } else {
            // `min(remaining)` keeps a rounded-up share from overshooting what
            // is left; the payment that follows would then take the zero.
            (total * *payment / paid).round_dp(2).min(remaining)
        };
        allocated += share;
        shares.push(share);
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn a_single_payment_receives_the_whole_refund() {
        assert_eq!(
            allocate_refund(dec!(120.00), &[dec!(300.00)]),
            vec![dec!(120.00)]
        );
    }

    #[test]
    fn a_full_refund_returns_each_payment_exactly() {
        let payments = [dec!(33.33), dec!(66.67)];
        assert_eq!(
            allocate_refund(dec!(100.00), &payments),
            vec![dec!(33.33), dec!(66.67)]
        );
    }

    #[test]
    fn a_partial_refund_is_proportional_to_the_payments() {
        let payments = [dec!(33.33), dec!(66.67)];
        assert_eq!(
            allocate_refund(dec!(10.00), &payments),
            vec![dec!(3.33), dec!(6.67)]
        );
    }

    #[test]
    fn a_cent_is_never_lost_to_rounding() {
        // Proportional rounding alone could leave the note a cent short; the
        // last share absorbs it whichever direction the strategy rounds.
        let shares = allocate_refund(dec!(0.01), &[dec!(50.00), dec!(50.00)]);
        assert_eq!(shares.iter().sum::<Decimal>(), dec!(0.01));
    }

    #[test]
    fn a_zero_share_stays_aligned_with_its_payment() {
        let payments = [dec!(0.50), dec!(99.50)];
        let shares = allocate_refund(dec!(0.01), &payments);
        assert_eq!(shares.len(), payments.len());
        assert_eq!(shares.iter().sum::<Decimal>(), dec!(0.01));
    }

    #[test]
    fn shares_sum_to_the_total_and_are_never_negative() {
        let payments = [dec!(10.00), dec!(20.00), dec!(30.00)];
        for total in [dec!(0.01), dec!(7.77), dec!(33.33), dec!(60.00)] {
            let shares = allocate_refund(total, &payments);
            assert_eq!(shares.iter().sum::<Decimal>(), total);
            assert!(shares.iter().all(|share| *share >= Decimal::ZERO));
        }
    }

    #[test]
    fn nothing_is_allocated_without_payments_or_total() {
        assert!(allocate_refund(dec!(10.00), &[]).is_empty());
        assert!(allocate_refund(dec!(0.00), &[dec!(50.00)]).is_empty());
        assert!(allocate_refund(dec!(10.00), &[dec!(0.00)]).is_empty());
    }

    #[test]
    fn a_line_reports_what_can_still_come_back() {
        let line = CreditableLine {
            product_id: Uuid::now_v7(),
            product_name: "Harina".into(),
            quantity: dec!(10),
            unit_price: dec!(1500.50),
            credited_quantity: dec!(4),
        };
        assert_eq!(line.returnable_quantity(), dec!(6));
    }

    #[test]
    fn a_line_subtotal_uses_the_frozen_price() {
        let item = CreditNoteItem {
            id: Uuid::now_v7(),
            product_id: Uuid::now_v7(),
            product_name: "Harina".into(),
            quantity: dec!(2.5),
            unit_price: dec!(1500.50),
        };
        assert_eq!(item.subtotal(), dec!(3751.25));
    }
}
