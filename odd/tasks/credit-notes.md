# Credit notes (customer returns)

Feature branch: `feat/credit-notes`. Goal: a customer returning part or all of a charged
sale is recorded as a credit note — its own document, linked to the sale, that returns stock
to the ledger and refunds money from the wallets that received it.

## Specs

S1 — A credit note selects any subset of the charged sale's lines with a quantity per line.
Several notes may be issued against one sale; the total credited quantity of a product never
exceeds the quantity sold. *(User decision: "Por ítem y cantidad (devolución parcial)".)*

S2 — Money leaves the same wallet(s) that received the payment for that sale, split in
proportion to the original payments, in cents, and the split sums exactly to the credit-note
total. Each refund movement keeps the link to the credited sale and to the original payment
method. *(User: "Sale de la misma wallet en que entró".)*

S3 — A credit note is its own document: id, link to the original sale, a required reason,
lines with the price frozen at the sale's price, a server-computed total, its actor and
timestamp. The original sale is never edited: no status change, no line change, no total
change. *(User: "Documento propio vinculado a la venta".)*

S4 — Returned quantities go back to stock, always, as new movements in the append-only
`stock_movements` ledger, written in the same transaction as the note. *(User: "Sí, siempre
vuelven al stock".)*

S5 — Only a `completed` sale can be credited. A `draft` or `cancelled` sale is rejected with
`409`; an unknown sale is `404`.

S6 — Validation and atomicity: quantity > 0; every product must be a line of that sale;
quantity ≤ still-returnable quantity; reason required after trimming, at most 200 characters;
an empty line list is rejected. Any rejection writes nothing: no note, no stock movement, no
wallet movement.

S7 — Reads: one endpoint returns a sale's credit notes plus, per sale line, the quantity
already credited and the quantity still returnable; creating a note returns the created
document.

S8 — Existing invariants hold: both ledgers stay append-only, a completed sale stays
immutable, decimals travel as strings, every money total is computed by the server, and the
client never dictates a total.

S9 — UI: a completed-sales screen lists charged sales with their credited total; each sale
opens a credit-note form (per-line quantities, reason, live totals from the server) and shows
the notes already issued. UI copy is English.

## Design

### Migrations

Two files, because PostgreSQL cannot use a new enum label in the transaction that adds it:

- `0009_credit_note_enums.sql`: `ALTER TYPE movement_reason ADD VALUE IF NOT EXISTS 'return';`
  and `ALTER TYPE wallet_movement_kind ADD VALUE IF NOT EXISTS 'sale_refund';` — nothing else.
- `0010_credit_notes.sql`: the tables and the constraint replacement below.

```sql
CREATE TABLE credit_notes (
  id uuid PRIMARY KEY,
  sale_id uuid NOT NULL REFERENCES sales(id) ON DELETE RESTRICT,
  total numeric(14,2) NOT NULL CHECK (total >= 0),
  reason text NOT NULL CHECK (length(btrim(reason)) > 0),
  created_by uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX credit_notes_sale_idx ON credit_notes(sale_id);
CREATE INDEX credit_notes_created_at_idx ON credit_notes(created_at DESC);

CREATE TABLE credit_note_items (
  id uuid PRIMARY KEY,
  credit_note_id uuid NOT NULL REFERENCES credit_notes(id) ON DELETE CASCADE,
  product_id uuid NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
  quantity numeric(12,3) NOT NULL CHECK (quantity > 0),
  unit_price numeric(12,2) NOT NULL CHECK (unit_price >= 0),
  UNIQUE (credit_note_id, product_id)
);
CREATE INDEX credit_note_items_product_idx ON credit_note_items(product_id);
```

`wallet_movements` constraint replacement (additive: every existing row still satisfies it):

```sql
ALTER TABLE wallet_movements DROP CONSTRAINT wallet_movements_sale_payment_is_complete;
ALTER TABLE wallet_movements DROP CONSTRAINT wallet_movements_only_sale_payment_references_a_sale;
ALTER TABLE wallet_movements ADD CONSTRAINT wallet_movements_sale_kind_is_complete
  CHECK (kind NOT IN ('sale_payment', 'sale_refund')
         OR (sale_id IS NOT NULL AND payment_method_id IS NOT NULL));
ALTER TABLE wallet_movements ADD CONSTRAINT wallet_movements_sale_link_only_for_sale_kinds
  CHECK (kind IN ('sale_payment', 'sale_refund') OR sale_id IS NULL);
```

### Ledger semantics

- `MovementReason::Return`: wire value `return`, sign `+1` (stock comes back).
- `WalletMovementKind::SaleRefund`: wire value `sale_refund`, sign `-1`, not manual.
- No stored balance: stock and wallet balances stay calculated sums of their ledgers.

### Refund allocation (pure function, unit-tested)

Given the note total `T` and the sale's payment amounts `p1..pn`, ordered by
`created_at, id`: for `i < n`, `share_i = round(T * p_i / Σp, 2)`; `share_n = T − Σ shares`.
Zero shares are dropped, never inserted. The shares always sum exactly to `T`.

### API

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/sales/{id}/credit-notes` | creates the note; `201` → `CreditNoteResponse` |
| GET | `/api/v1/sales/{id}/credit-notes` | `SaleCreditNotesResponse` |

DTO names are part of the contract:

```
CreateCreditNoteRequest { reason: String, items: [{ product_id: Uuid, quantity: Decimal }] }
CreditNoteResponse {
  id, sale_id, reason, total: String, created_at,
  items:   [{ product_id, product_name, quantity: String, unit_price: String, subtotal: String }],
  refunds: [{ wallet_id, wallet_name, payment_method_id, payment_method_name, amount: String }],
}
SaleCreditNotesResponse {
  sale_id, sale_total: String, refunded_total: String,
  items: [{ product_id, product_name, quantity: String, unit_price: String,
            credited_quantity: String, returnable_quantity: String }],
  credit_notes: [CreditNoteResponse],
}
```

Errors follow the sales module's mapping: `404 not_found` for an unknown sale,
`409 sale_not_creditable` for a sale that is not completed, `409` with the exact available
quantity for a quantity above the returnable one (same payload shape as the existing
insufficient-stock conflict), `422` for an empty line list, a product that is not a line of
the sale, a quantity of zero or less, and a blank or over-long reason. `500` for internal
errors, logged not serialized.

### Module layout

`src/credit_notes/{mod.rs, model.rs, repository.rs, service.rs, handlers.rs}`, merged in
`src/routes.rs`, documented in `src/openapi.rs` (two paths, the schemas above, a
`credit_notes` tag). The create transaction locks the sale `FOR UPDATE`, re-checks the
returnable quantity inside the transaction, inserts the note, the lines, the `return` stock
movements and the `sale_refund` wallet movements, and commits once.

## Tasks

- **T1** — S1–S8 — migrations `0009` + `0010`, `return` reason, `sale_refund` kind, model
  (types + allocation) with unit tests — worker — commit
  `feat(credit-notes): add the ledger kinds and schema`.
- **T2** — S1–S8 — repository + service + handlers + routes + OpenAPI + `.sqlx` cache, with
  `tests/credit_notes.rs` — worker — commit
  `feat(credit-notes): issue a credit note and refund the payment wallets`.
- **T3** — S7 — the read endpoint and its tests — worker — commit
  `feat(credit-notes): report the credits of a sale`.
- **T4** — S8 — `docs/credit-notes.md`, `docs/sales.md`, `docs/payments.md`, README and
  `make fe-schema` — worker — commit `docs(credit-notes): document returns and refunds`.
- **T5** — S9 — frontend completed-sales list, credit-note form, client methods, e2e proof —
  worker — commit `feat(ui): issue credit notes from a completed sale`.
- **T6** — S1–S9 — independent verification and native review — parent.

## Log

L1 — user (verbatim): "trabajemos en las notas de crédito, que serían cuando un cliente
devuelve una venta."

L2 — questionnaire answers (verbatim option labels): "Por ítem y cantidad (devolución
parcial)"; "Sale de la misma wallet en que entró"; "Documento propio vinculado a la venta";
"Sí, siempre vuelven al stock".
