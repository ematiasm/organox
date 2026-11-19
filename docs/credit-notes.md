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
   of the first one.
2. **Returned goods always go back to stock.** Every credited line writes a new
   `stock_movements` row with reason `return`, in the same transaction as the
   note. `stock_movements` stays append-only: stock is still the signed sum of
   its ledger, not a column to adjust.
3. **Money leaves the wallets that received it.** The sale's original payments
   decide the split: each share is computed in cents in proportion to the
   amount that went into that wallet, and the last payment absorbs the rounding
   remainder so the shares add up to the note total exactly. Zero shares are
   never written. Every `sale_refund` row names the sale it reverses, the
   payment method that collected the money and the credit note that caused it,
   so a reader can follow the refund without re-deriving the split.
4. **The total is the server's number.** The note copies the `unit_price`
   frozen on each `sale_items` line, computes the subtotals and the total, and
   stores it. The client sends only a reason and `{product_id, quantity}`
   lines: no price, no total, no way to decide what the customer is owed.
5. **Only a `completed` sale can be credited.** A draft or a cancelled sale is
   `409 sale_not_creditable`; an unknown sale is `404`. An empty line list, a
   product that is not on the sale, a non-positive quantity, a quantity above
   what is still returnable, or a blank or over-long reason is rejected with
   `422`/`409`. A rejected note writes nothing: no note, no stock movement, no
   wallet movement. The create transaction locks the sale, re-checks what is
   left to return and commits once.

The sale's own total, lines and status stay untouched throughout. The note is
the evidence of what came back and where the money went.

## API

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/sales/{id}/credit-notes` | Creates the note from `{reason, items: [{product_id, quantity}]}`; `201` → `CreditNoteResponse` |
| GET | `/api/v1/sales/{id}/credit-notes` | `200` → `SaleCreditNotesResponse`: the charged lines, what is still returnable and every note already issued |

Both routes require a session. Decimals are strings in both directions; money
has exactly two decimals and quantities drop trailing zeros.

`CreditNoteResponse` fields:

- `id`, `sale_id`, `reason`, `total`, `created_at`.
- `items[]`: `product_id`, `product_name`, `quantity`, `unit_price`, `subtotal`.
- `refunds[]`: `wallet_id`, `wallet_name`, `payment_method_id`,
  `payment_method_name`, `amount`.

`SaleCreditNotesResponse` fields:

- `sale_id`, `sale_total`, `refunded_total`.
- `items[]`: `product_id`, `product_name`, `quantity`, `unit_price`,
  `credited_quantity`, `returnable_quantity`.
- `credit_notes[]`: `CreditNoteResponse`.

## What is missing

- **Fiscal numbering**: a note has an internal id, not a numbered fiscal
  document.
- **Customer**: sales and notes are not attached to a customer.
- **Credit balance in favor of the customer**: a note always refunds the
  original wallets; money cannot stay as store credit.
- **VAT breakdown**: totals are stored without a tax breakdown.
