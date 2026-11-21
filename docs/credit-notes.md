# Credit notes

## The central decision: the note is a document, the sale is a fact

A completed sale is never edited, cancelled or re-opened. A return is recorded
as a **credit note**: its own document, linked to the sale, with its own lines,
reason, stock movements and wallet movements.

Why:

1. **One sale can be returned several times.** Each note credits a subset of
   the charged lines, with a quantity per line. Several notes may be issued
   against the same sale; summing what each note credited can never exceed the
   quantity sold per product. A second return is another document, not a rewrite
   of the first one. The same ceiling applies to money: the refunds of all notes
   can never add up to more than the sale charged, however many notes the return
   is split into. Per-note cent rounding must not turn a charged 66.66 into 70.00
   of refunds; a note that would cross that line is rejected with a 409 that
   names the money allowance left, and the note is refused whole — never
   clamped into a smaller one.
2. **Returned goods always go back to stock.** Every credited line writes a new
   `stock_movements` row with reason `return`, in the same transaction as the
   note. `stock_movements` stays append-only: stock is still the signed sum of
   its ledger, not a column to adjust.
3. **Debt is cancelled before cash is refunded.** The note first cancels
   what the sale still owes on the customer's account — `min(note,
   outstanding)` written as one `credit_note` entry, nothing when the sale
   owes nothing — and only the remainder is refunded as cash, through the
   tenders that actually collected money for that sale: its original
   `sale_payment` movements, then the receipts whose allocations paid it,
   each capped at what it collected. The split is computed in integer cents
   in proportion to what each tender collected, with the rounding leftovers
   going to the largest fractional remainders, and the shares add up to the
   cash part exactly — computed cumulatively across the sale's notes, so
   three notes of 33.33 on a 100 sale refund 99.99, never a cent more. A
   fully unpaid credit sale therefore produces no wallet movement at all:
   no wallet ever took any money for it. Zero shares are never written, and
   a note whose cash part no tender collected is refused whole. Every
   `sale_refund` row names the sale it reverses, the payment method that
   collected the money and the credit note that caused it, so a reader can
   follow the refund without re-deriving the split.
4. **The total is the server's number.** The note copies the `unit_price`
   frozen on each `sale_items` line, computes the subtotals and the total, and
   stores it. The client sends only a reason and `{product_id, quantity}`
   lines: no price, no total, no way to decide what the customer is owed.
5. **Only a `completed` sale can be credited.** A draft or a cancelled sale is
   a `409` whose detail says the sale is not completed; an unknown sale is
   `404`. An empty line list, a product that is not on the sale, a
   non-positive quantity, a blank or over-long reason is `422`; a quantity
   above what is still returnable is a `409` whose detail names what is left,
   and so is a note whose refunds would cross the sale total, naming the money
   allowance left.
   Errors are RFC 7807 documents: a client matches the status and shows the
   `detail`, never a machine code. A rejected note writes nothing: no note, no
   stock movement, no wallet movement. The create transaction locks the sale,
   re-checks what is left to return and the total already refunded, and commits
   once.

The sale's own total, lines and status stay untouched throughout. The note is
the evidence of what came back and where the money went.

## API

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/sales/{id}/credit-notes` | Creates the note from `{reason, items: [{product_id, quantity}], idempotency_key?}`; `201` → `CreditNoteResponse`, or `200` with the same document when the key was already used |
| GET | `/api/v1/sales/{id}/credit-notes` | `200` → `SaleCreditNotesResponse`: the charged lines, what is still returnable and every note already issued |

Both routes require a session. Decimals are strings in both directions; money
has exactly two decimals and quantities drop trailing zeros.

`idempotency_key` is optional. The server trims it; a blank key or one longer
than 100 characters is `422`. A key is unique per sale: resending a create with
the same key returns the first note with `200` and writes nothing new — no
second document, no second stock movement, no second refund. A fresh key (or no
key) produces a new note with `201`. Retrying with the same key is safe even if
the first response was lost or the two requests arrive at the same time.

`CreditNoteResponse` fields:

- `id`, `sale_id`, `reason`, `total`, `created_at`.
- `items[]`: `product_id`, `product_name`, `quantity`, `unit_price`, `subtotal`.
- `refunds[]`: `wallet_id`, `wallet_name`, `payment_method_id`,
  `payment_method_name`, `amount`.

`SaleCreditNotesResponse` fields:

- `sale_id`, `sale_total`, `refunded_total` (the notes' refunds, never more
  than `sale_total`), `refundable_total` (`sale_total − refunded_total`:
  the money allowance create still enforces, so a client sees the same limit
  the 409 raises).
- `items[]`: `product_id`, `product_name`, `quantity`, `unit_price`,
  `credited_quantity`, `returnable_quantity`.
- `credit_notes[]`: `CreditNoteResponse`.

Quantities and money are two budgets, and a fractional sale can exhaust them
together before either is fully spent: 0.002 units still returnable against
0.04 of allowance, when the smallest note the price allows is worth 0.07. The
cap refuses rather than clamps — a note's total is always the sum of its
lines — so `refundable_total` is the honest number a client should show.
Once the allowance reaches zero, further notes are refused even if units
remain. The residue such a sale is left with — a money allowance smaller
than the smallest note the prices allow — is permanent: nothing in the
credit-note flow can refund it, so it stays on the sale until an operator
writes it off by hand. A close-out flow that resolves the residue is
explicitly future work.

## What is missing

- **Cent-exact split over three or more tenders**: the cash surplus of a note is
  split by largest remainder across the tenders that collected the sale. With
  three or more tenders that allocation is not componentwise monotone in the
  cumulative cash (the Alabama paradox), so a later note whose surplus lands
  exactly on one of those steps is refused by the split's own safety check and
  answers `500` even though every tender still has room. The refusal is
  fail-safe: no wallet movement is written and no tender is ever refunded more
  than it collected. Sales with one or two tenders are unaffected. Allocating
  each note against the remaining capacity instead of subtracting two
  cumulative splits is the future fix.
- **Fiscal numbering**: a note has an internal id, not a numbered fiscal
  document.
- **Customer on the note**: the sale may name a customer (`docs/customers.md`),
  but the note itself carries no customer reference or name; there is no
  per-customer view of returns.
- **Credit balance in favor of the customer**: a note always refunds the
  original wallets; money cannot stay as store credit.
- **VAT breakdown**: totals are stored without a tax breakdown.
