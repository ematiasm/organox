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

S9 — UI: a completed-sales screen lists charged sales; each sale opens a credit-note
form (per-line quantities, reason, live totals from the server) and shows the notes
already issued, including how much of the sale has been refunded. UI copy is English.

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

### Refund linkage

A `sale_refund` row names three things: the sale it reverses, the payment
method that collected the money, and — added in `0011` — the `credit_note_id`
that caused it. The first two already exist on `sale_payment`; the third is what
lets the read endpoint show, per note, where its money actually went instead of
re-deriving a split that may no longer match. A refund without its note is
rejected by CHECK, and no other kind may carry a note id.

### Module layout

`src/credit_notes/{mod.rs, model.rs, repository.rs, service.rs, handlers.rs}`, merged in
`src/routes.rs`, documented in `src/openapi.rs` (two paths, the schemas above, a
`credit_notes` tag). The create transaction locks the sale `FOR UPDATE`, re-checks the
returnable quantity inside the transaction, inserts the note, the lines, the `return` stock
movements and the `sale_refund` wallet movements, and commits once.

## Tasks

- **T1** — S1–S8 — migrations `0009` + `0010`, `return` reason, `sale_refund` kind, model
  (types + allocation) with unit tests — worker — done in `c7672cd`
  (`feat(credit-notes): add the ledger kinds and schema`).
- **T2** — S1–S8 — repository + service + handlers + routes + OpenAPI + `.sqlx` cache, with
  `tests/credit_notes.rs` — worker — done in `f3ea60f`
  (`feat(credit-notes): issue a credit note and refund the payment wallets`).
- **T3** — S7 — the read endpoint and its tests — worker — done in `70fa313`
  (`feat(credit-notes): read a sale's credits and trace each refund to its note`).
- **T4** — S8 — `docs/credit-notes.md`, `docs/sales.md`, `docs/payments.md`, README and
  `make fe-schema` — worker — done in `4b5eaf5`
  (`docs(credit-notes): document returns, refunds and the new endpoints`).
- **T5** — S9 — frontend completed-sales list, credit-note form, client methods, e2e proof —
  worker — done in `0461da5` (`feat(ui): issue credit notes from a completed sale`).
- **T6** — S1–S9 — independent verification and native review — parent.
- **T7** — S2, S3, S6, S7 — the slice-2 review advisories worth acting on: cap the cumulative
  refund of a sale at what it charged, accept an optional idempotency key on create (a retry
  returns the same note), and add the missing tests (concurrent last unit, fractional
  quantities, HTTP 404/422) — worker — commit `fix(credit-notes): cap the cumulative refund
  and make create idempotent`.

Slices are committed in this order, each one reviewable on its own: plan and
spec (1 commit); ledger kinds and schema; the create endpoint with its tests;
the read endpoint and the refund link; docs plus regenerated artifacts; the UI.
Past ~400 authored lines per slice, publish as chained PRs rather than one
delivery.

## Log

L1 — user (verbatim): "trabajemos en las notas de crédito, que serían cuando un cliente
devuelve una venta."

L2 — questionnaire answers (verbatim option labels): "Por ítem y cantidad (devolución
parcial)"; "Sale de la misma wallet en que entró"; "Documento propio vinculado a la venta";
"Sí, siempre vuelven al stock".

L3 — architect decision (not a user request): a `sale_refund` movement also names its
credit note through `wallet_movements.credit_note_id` (migration `0011`), so the read
endpoint can report each note's refunds instead of re-deriving the split from the sale.

L4 — docs slice: `docs/credit-notes.md` plus the sales, payments and README updates,
and the regenerated OpenAPI artifacts. `make check` reports 204 tests. The branch is
past one review slice, so delivery will be chained PRs rather than one branch-wide
review.

L5 — UI slice: `/operations/sales` and its credit-note form, typed client methods, the
rail entry and the nav assertion that now matches the rail. Proof: `frontend/e2e/sales.cjs`
22/22, `nav-shell.cjs` 19/19, `make fe-check` 0 errors, `make fe-build` ok. `payments.cjs`
P6 fails for an environmental reason, not this change: the dev database already holds an
active method named `Efectivo` that no probe created, so the POS still offers a method
after the probe deactivates its own; the script and that data were left untouched.

L6 — requirement correction after independent verification (architect, not the human):
S9 asked the sales list to show each sale's credited total; the reviewed design shows the
charged total in the list and the credited total on the return screen, because
`GET /sales` does not expose credits and a per-row request would be an N+1. S9 now says
that. Same verification round: `docs/credit-notes.md` named a `409 sale_not_creditable`
identifier that never exists on the wire, which the doc no longer claims; errors are RFC
7807 with a status and a `detail`.

L7 — native review of slice 1 (tier high, lenses risk + reliability, lineage
`review-718441ff157bfe52`): both lens artifacts were admitted after two admission retries
(one echoed a different artifact subject, one listed an inspection path outside the frozen
candidate); the refuter then ran and closed `correction_required` on one CRITICAL finding,
`R3-sale-payment-check` (`tests/payments.rs:610-614`). The claim does not hold: the nested
branches gave `sale_payment` its sale and method in the first branch, and `0008` already
required that link, so `0010` is additive as it states; the suite was green throughout. The
correction was accepted anyway (user decision): the linkage is now one flat `match` per
kind, 38 diff lines, committed `9e6bc30`, with the rest of the chain rebased onto it; the
final tree differs from the verified one only in that hunk.

The lineage cannot advance: its correction-plan slot reoffers a provider-issued binding
(`--target=sha256:6e842b…`, `--expected-revision=sha256:9b99cc…`) that the capture tool
rejects as "missing or stale for current STATUS" after three submissions (canonical object,
envelope object, exact serialized string), while STATUS reports the corrected candidate
(`target_identity=sha256:73ff73…`). A facade defect, not a repository problem: nothing was
approved and no delivery action was taken.

Delivery branches, chained in this order: `feat/credit-notes-s1-ledger` `9e6bc30`,
`-s2-create` `c66f0ec`, `-s3-read` `3370ba3`, `-s4-docs` `26162fa`, `-s5-ui` `d0132cc`.
`feat/credit-notes` is the last of them.

L8 — native review of slice 2 (`c66f0ec` vs `9e6bc30`, tier medium, lenses risk +
resilience + reliability): **approved**; authority burned with the exact acknowledgement.
One lens artifact was refused at admission (`out_of_scope`, same relay defect as slice 1) and
admitted on the provider-prescribed retry. Seven non-blocking advisories stand as later work,
none of them reopening the candidate: `R1-uncapped-refund-rounding` (per-note rounding can,
with many sub-cent notes, refund more than the sale charged), `R1-wallet-rounding-drift`
(cents of per-wallet drift across notes), `R3-refund-share-drop` (claims a negative remainder
share; `allocate_refund` clamps every share with `min(remaining)`, so the remainder cannot go
negative and the claim does not hold), `R3-fractional-quantity-rounding`,
`R3-http-status-coverage` (404/422 unasserted over HTTP), `R4-concurrency-guard-untested` (the
committed suite has no concurrent-create test; the independent verifier proved it out of
tree), `R4-create-retry-duplicates-note` (no idempotency key on create).

L9 — user decision after the slice-2 approval (verbatim): "ok, segui con 1", which is the
follow-up slice: `R1-uncapped-refund-rounding`, `R4-create-retry-duplicates-note` and the three
missing tests. `R1-wallet-rounding-drift` stays accepted as documented design (the per-note
allocation is what the spec asked for) and `R3-refund-share-drop` is refuted, not worked.
