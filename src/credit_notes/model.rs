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
use rust_decimal::prelude::ToPrimitive;
use uuid::Uuid;

/// Longest accepted reason, in characters after trimming.
pub const MAX_REASON_LEN: usize = 200;

/// Longest accepted idempotency key, in characters after trimming.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

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
    /// The line's money, rounded to the cent like the note total it feeds.
    ///
    /// A fractional quantity can produce more than two decimals (0.333 x
    /// 3.00 = 0.999); showing 0.99 next to a note total of 1.00 would make
    /// the document contradict itself.
    pub fn subtotal(&self) -> Decimal {
        crate::money::line_subtotal(self.quantity, self.unit_price)
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

/// What create returns: the document plus whether the request was a replay.
///
/// The flag describes the request, not the stored note, so it is kept out of
/// `CreditNoteDetail`, which reads and writes use everywhere else.
#[derive(Debug, Clone)]
pub struct CreatedCreditNote {
    pub detail: CreditNoteDetail,
    pub replayed: bool,
}

impl std::ops::Deref for CreatedCreditNote {
    type Target = CreditNoteDetail;

    fn deref(&self) -> &Self::Target {
        &self.detail
    }
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

/// A line the caller asks to return: which product and how many units.
/// The price is not part of it; the sale's own price decides the money.
#[derive(Debug, Clone, Copy)]
pub struct ReturnLine {
    pub product_id: Uuid,
    pub quantity: Decimal,
}

/// Split `total` across tenders in proportion to each tender's remaining
/// refundable capacity (`capacities`).
///
/// The result is aligned one-to-one with `capacities`, so the caller knows
/// which wallet each share belongs to. The arithmetic runs in integer cents:
/// each share is the floor of its proportional part, and the cents the floors
/// lost go to the largest fractional remainders, ties broken by index. No
/// share is rounded on its own — per-share rounding is how a sequence of notes
/// drifts past what the sale charged — and no single tender absorbs the whole
/// remainder, so the split never depends on which tender happens to be last.
///
/// Because callers pass each tender's remaining capacity (`collected -
/// prior_refunded`) rather than subtracting two cumulative splits, every
/// partial note produces strictly non-negative shares for any number of
/// tenders (avoiding the Largest Remainder Alabama paradox on 3+ tenders).
///
/// The shares add up to `total` exactly. When `total` is at most the sum of
/// `capacities` — the only case the refund path allows — no share exceeds its
/// tender's remaining capacity either. Beyond that the shares are capped at
/// each capacity, the sum falls short of `total`, and the caller refuses the
/// note rather than refunding money nobody paid.
///
/// A share of zero stays in place, so the alignment survives, but it is never
/// written to the ledger (`wallet_movements.amount > 0`). An empty list has
/// no alignment to keep, so it returns no shares at all; a list with nothing
/// to split over returns aligned zeros.
pub fn split_cash_refund(total: Decimal, capacities: &[Decimal]) -> Vec<Decimal> {
    if capacities.is_empty() {
        return Vec::new();
    }

    // Money here never carries more than two decimals (`numeric(14, 2)`), so
    // the shift to cents is exact; the truncation only defends a caller that
    // breaks that rule, flooring instead of inventing a fraction of a cent.
    let cents =
        |amount: Decimal| -> i128 { (amount * Decimal::from(100)).trunc().to_i128().unwrap_or(0) };

    let total_cents = cents(total);
    let caps: Vec<i128> = capacities.iter().copied().map(cents).collect();
    let sum_caps: i128 = caps.iter().sum();

    // Nothing to split, or nothing any tender collected.
    if total_cents <= 0 || sum_caps <= 0 {
        return vec![Decimal::ZERO; caps.len()];
    }

    // The floor of each proportional share, and the fractional remainder it
    // lost. Scaled by `sum_caps` once, so every division below is the same
    // denominator and the remainders are comparable with each other.
    let mut shares = Vec::with_capacity(caps.len());
    let mut remainders = Vec::with_capacity(caps.len());
    for cap in &caps {
        let scaled = total_cents * cap;
        shares.push(scaled / sum_caps);
        remainders.push(scaled % sum_caps);
    }

    // The floors together lost at most one cent per tender; hand those cents
    // back one by one to the largest fractional remainders, ties broken by
    // index. Deterministic, and it keeps the split stable across notes.
    let leftover = (total_cents - shares.iter().sum::<i128>()) as usize;
    let mut by_remainder: Vec<usize> = (0..caps.len()).collect();
    by_remainder.sort_by(|&a, &b| remainders[b].cmp(&remainders[a]).then(a.cmp(&b)));
    for &index in by_remainder.iter().take(leftover) {
        shares[index] += 1;
    }

    // The cap is the promise: a tender never hands back more than it took.
    // It only bites when `total` exceeds what was collected, which the refund
    // path never allows — the caller's sum check turns it into a refusal.
    shares
        .into_iter()
        .zip(caps)
        .map(|(share, cap)| Decimal::from_i128_with_scale(share.min(cap), 2))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn a_single_tender_takes_the_whole_refund() {
        assert_eq!(
            split_cash_refund(dec!(120.00), &[dec!(300.00)]),
            vec![dec!(120.00)]
        );
    }

    #[test]
    fn two_tenders_split_proportionally() {
        // A third of the money came from the first tender, two thirds from
        // the second, and the split says exactly that.
        assert_eq!(
            split_cash_refund(dec!(100.00), &[dec!(100.00), dec!(200.00)]),
            vec![dec!(33.33), dec!(66.67)]
        );
    }

    #[test]
    fn a_full_refund_returns_each_tender_exactly() {
        assert_eq!(
            split_cash_refund(dec!(100.00), &[dec!(40.00), dec!(60.00)]),
            vec![dec!(40.00), dec!(60.00)]
        );
    }

    #[test]
    fn cent_remainders_go_to_the_largest_fractional_parts() {
        // One cent over two tenders: the second's fractional part is larger,
        // so the cent is its share — not whichever tender happens to be
        // first or last.
        assert_eq!(
            split_cash_refund(dec!(0.01), &[dec!(1.00), dec!(2.00)]),
            vec![dec!(0.00), dec!(0.01)]
        );

        // One cent over three equal tenders: the remainders tie, so the
        // lowest index decides, and the choice is stable.
        assert_eq!(
            split_cash_refund(dec!(1.00), &[dec!(1.00), dec!(1.00), dec!(1.00)]),
            vec![dec!(0.34), dec!(0.33), dec!(0.33)]
        );
    }

    #[test]
    fn the_shares_sum_exactly_to_the_total() {
        let tenders = [dec!(10.00), dec!(20.00), dec!(30.00), dec!(0.50)];
        for total in [
            dec!(0.01),
            dec!(0.67),
            dec!(7.77),
            dec!(33.33),
            dec!(60.49),
            dec!(60.50),
        ] {
            let shares = split_cash_refund(total, &tenders);
            assert_eq!(shares.iter().copied().sum::<Decimal>(), total);
            assert!(shares.iter().all(|share| *share >= Decimal::ZERO));
        }
    }

    #[test]
    fn a_share_never_exceeds_what_its_tender_collected() {
        // Full capacity is the boundary: every share lands exactly on its
        // cap without being clamped.
        let tenders = [dec!(40.00), dec!(60.00)];
        for total in [dec!(40.00), dec!(99.99), dec!(100.00)] {
            for (share, cap) in split_cash_refund(total, &tenders).into_iter().zip(tenders) {
                assert!(share <= cap, "{share} exceeded its tender's {cap}");
            }
        }

        // Beyond capacity the shares are capped and the sum falls short —
        // the caller's job to refuse, not the split's to invent.
        assert_eq!(
            split_cash_refund(dec!(150.00), &[dec!(100.00)]),
            vec![dec!(100.00)]
        );
    }

    #[test]
    fn sequential_remaining_capacity_splits_hold_note_after_note() {
        // Three notes of 33.33 against two tenders of 50 each, applied the
        // way the refund path applies them: each note allocates against each
        // tender's remaining refundable capacity.
        let mut remaining = [dec!(50.00), dec!(50.00)];
        let mut refunded_per_tender = [Decimal::ZERO, Decimal::ZERO];

        for _ in 0..3 {
            let shares = split_cash_refund(dec!(33.33), &remaining);

            // No tender is ever asked to hand back a cent it does not have
            // left, and the shares equal this note's cash to the cent.
            assert_eq!(shares.iter().copied().sum::<Decimal>(), dec!(33.33));
            for ((rem, refunded), share) in remaining
                .iter_mut()
                .zip(refunded_per_tender.iter_mut())
                .zip(shares)
            {
                assert!(share >= Decimal::ZERO, "negative share: {share}");
                assert!(share <= *rem, "share {share} exceeded remaining {rem}");
                *rem -= share;
                *refunded += share;
            }
        }

        // 99.99 out of the 100 the sale charged — never a cent more — and
        // no tender refunded beyond what it collected.
        assert_eq!(refunded_per_tender[0], dec!(50.00));
        assert_eq!(refunded_per_tender[1], dec!(49.99));
        assert_eq!(remaining, [dec!(0.00), dec!(0.01)]);
    }

    #[test]
    fn three_tenders_partial_refunds_avoid_alabama_paradox() {
        // Classic Alabama paradox case (#95): $7.00 paid via [$3, $3, $1].
        // Cumulative Largest Remainder gives [0.01, 0.01, 0.01] at $0.03 and
        // [0.02, 0.02, 0.00] at $0.04, producing a -0.01 increment on tender 3.
        // Splitting against remaining capacity stays non-negative on every step
        // all the way through full drainage.
        let mut remaining = [dec!(3.00), dec!(3.00), dec!(1.00)];

        for step in [dec!(0.03), dec!(0.01), dec!(0.01), dec!(6.95)] {
            let shares = split_cash_refund(step, &remaining);
            assert_eq!(shares.iter().copied().sum::<Decimal>(), step);
            for (rem, share) in remaining.iter_mut().zip(shares) {
                assert!(share >= Decimal::ZERO);
                assert!(share <= *rem);
                *rem -= share;
            }
        }
        assert_eq!(remaining, [dec!(0.00), dec!(0.00), dec!(0.00)]);

        // Stress-test every single 1-cent step across the entire $7.00.
        let mut remaining = [dec!(3.00), dec!(3.00), dec!(1.00)];
        for _ in 0..700 {
            let shares = split_cash_refund(dec!(0.01), &remaining);
            assert_eq!(shares.iter().copied().sum::<Decimal>(), dec!(0.01));
            for (rem, share) in remaining.iter_mut().zip(shares) {
                assert!(share >= Decimal::ZERO);
                assert!(share <= *rem);
                *rem -= share;
            }
        }
        assert_eq!(remaining, [dec!(0.00), dec!(0.00), dec!(0.00)]);
    }

    #[test]
    fn an_empty_or_zero_capacity_list_refunds_nothing() {
        // No tenders: no alignment to keep.
        assert!(split_cash_refund(dec!(10.00), &[]).is_empty());
        // Tenders that collected nothing, or nothing to split: aligned
        // zeros, so a cumulative subtraction over them stays aligned.
        assert_eq!(
            split_cash_refund(dec!(10.00), &[dec!(0.00), dec!(0.00)]),
            vec![Decimal::ZERO, Decimal::ZERO]
        );
        assert_eq!(
            split_cash_refund(dec!(0.00), &[dec!(50.00)]),
            vec![Decimal::ZERO]
        );
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
