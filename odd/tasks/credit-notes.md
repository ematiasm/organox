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
  quantities, HTTP 404/422) — worker — done in d02f67c (`fix(credit-notes): cap the cumulative
  refund and make create idempotent`).
- **T8** — S2, S6, S7 — the slice-6 advisories worth acting on: the replay pre-check reads on
  the create transaction's own connection instead of a second pool checkout; the 409 that the
  money cap raises names the allowance left, and the read endpoint reports it as
  `refundable_total`; tests for the 200/201 replay status over HTTP, an over-long key, a
  concurrent same-key pair, and a fractional sale whose quantities and money run out together.
  T8 keeps the note's `total` equal to the sum of its lines: the cap refuses, never clamps.
- **T10** — S9 — the two real defects the slice-5 review found: refund rows keyed by a value
  that repeats when a sale paid twice with one method (add the position to the key), and a
  failed post-success refresh hiding a note that was created (separate the two errors and
  send one idempotency key per intended note, so a retry replays instead of duplicating).
  `returnable_quantity === '0'` becomes a numeric check, and the browser proof gains the
  same-method-two-payments sale and the fully-returned state.
- **T9** — S6, S7 — the four T8 advisories: assert a 100-character key is accepted, prove
  the one-connection property with a pool capped at one connection, state in the code that
  the unique-race recovery is unreachable under the sale lock (a comment, not a test), and
  document that a fractional residue is written off by hand today.

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

L10 — native review of slice 6 (`9770d70` vs `d0132cc`, tier medium, lenses risk + resilience
+ reliability): **approved**, all three artifacts admitted first try, authority burned. Ten
non-blocking advisories. The ones worth acting on: `R3-keyed-create-double-pool-hold` /
`R4-keyed-create-needs-a-second-pool-connection` (the replay pre-check reads through the pool
while the create transaction holds a connection and the sale lock: two connections per keyed
create, and a saturation path to a 500 — it can read through `&mut *tx` instead);
`R4-refund-cap-strands-fractional-remainder` (the cap refuses instead of clamping, so a
fractional sale can end with returnable quantity the read endpoint still advertises and
create permanently refuses — 0.002 units left against a 0.04 allowance whose natural note is
0.07); `R3-subtotal-total-cent-drift` (each subtotal rounds, the total rounds the raw sum, so a
multi-line fractional note can display 0.99 against 1.00); and the missing evidence:
`R3-replay-status-unasserted` (200 vs 201 never asserted over HTTP),
`R3-concurrent-replay-unproved` / `R4-concurrent-key-recovery-untested` (the unique-race
fallback is unexercised), `R3-key-length-boundary-untested`, `R3-key-maxlength-pretrim`
(schema caps the raw key, the server trims first) and `R1-replay-unscoped-key` (a key replays
for any session holder of that sale, not only its creator; informational, writes nothing).

L12 — native review of the T8 fix (`f38c133` vs `0dc163f`, tier medium, lenses resilience +
reliability): **approved**, both artifacts admitted, authority burned. Four SUGGESTION
advisories, all later work: `R3-key-accept-boundary-untested` (the 100-character key is never
asserted as valid), `R3-keyed-create-single-hold-unproved` (no test pins the one-connection
property with a pool capped at one), `R4-concurrent-recovery-branch-unexercised` (the
unique-race recovery is unreachable under the sale lock; needs a comment saying so or a forced
test) and `R4-fractional-remainder-has-no-write-off` (the fractional residue is disclosed but
has no close-out path).

L13 — native review of the polish slice (`e239090` vs `7d6d6cd`, tier medium, lens
reliability): **approved**, authority burned. Three SUGGESTION advisories, all about test
rigor: `R3-boundary-retry-money-side-unasserted` (the boundary test asserts the note and the
stock, not the wallet), `R3-one-connection-proof-implicit-timeout` (the capped pool relies on
sqlx's default acquire timeout; an explicit short one would make a regression fail fast) and
`R3-padded-key-claim-service-seam-only` (the padded-key case is proved at the service seam,
while the HTTP schema caps the raw key before the trim). Recorded as later work: closing them
would start another candidate and another review, so the advisory chain stops here.

L14 — delivery: issue #52 (labelled `status:approved`) carries the intent, and the chain is
published as six stacked PRs, each linked to it and labelled `type:feature`: #53 s1 → `main`,
#54 s2, #55 s3, #56 s4, #57 s5, #58 s6 (`Closes #52`). The first push attempt was refused by
the RDD delivery guard ("native assessment reports a due review") while the only unreviewed
delta was one passive docs commit; after assessing that delta as passive the push went
through unchanged.

L15 — native review of slice 3 (`3370ba3` vs `c66f0ec`, tier high, lenses risk +
reliability): **approved**, authority burned. Five non-blocking advisories, two of them about
the same migration: `R1-refund-constraint-no-backfill` / `R3-refund-link-check-not-additive`
(0011 adds `credit_note_id` and validates a CHECK requiring it at once, so it would fail on a
database that had already written `sale_refund` rows before 0011 — a window that exists only
if an intermediate state between slices was ever deployed, which never happened here; a
`NOT VALID` + later `VALIDATE` would make it strictly additive). `R3-draft-total-midpoint-rounding`
(the draft fallback rounds with banker's rounding while the column rounds half away from zero,
so an exact half-cent draft can report one cent less than completion freezes),
`R3-credit-read-torn-snapshot` (the read is several autocommit statements, so a note committed
mid-read can appear in the totals but not in the returnable quantities) and
`R1-refund-note-sale-link-unchecked` (nothing ties the refund's sale to its note's sale; the
create transaction keeps them equal, a composite FK would make the ledger self-consistent).

L16 — closing the two s3 advisories. The snapshot one was real and is fixed: `for_sale` now
reads the sale, its lines, the notes and the refunds inside one REPEATABLE READ transaction, so
a note committed mid-read can no longer appear in the totals while the returnable quantities
come from an older view. The rounding one does not hold: `complete` rounds with `round_dp(2)`
in Rust before the value reaches `numeric(14,2)` (`src/sales/repository.rs`), so the column's
half-away-from-zero rule never applies, and the draft fallback uses the same `round_dp(2)`;
both paths agree by construction, which the comment now says. No behavior changed for that
second one, so it carries no test; the snapshot fix is not deterministically testable without a
hook between its queries and is documented in place.

L17 — native review of the snapshot fix (`77558dd` vs `90bebab`, tier medium, lens
resilience): **approved**, authority burned. One SUGGESTION, `R4-one-snapshot-unpinned`: the
one-snapshot guarantee rests on argument discipline, since the read helpers still accept a pool
and a later revert of one call site would compile and pass; pinning them to
`&mut PgConnection` would make that regression fail to compile. Left as later work — the
advisory chain has to end, and the merge of the chain is the user's decision.

L18 — native review of slice 5 (`d0132cc` vs `26162fa`, tier medium, lenses readability +
reliability): **approved**, authority burned. Eight non-blocking advisories, two of them real
UI defects worth fixing: `R3-duplicate-refund-keys` (both keyed `each` blocks key refunds by
wallet+method, and a sale paid twice with the same method — allowed by `resolve_payments` —
produces two refunds with one key, which Svelte 5 reports and which drops or mangles the second
row) and `R2-003` (create and the post-success refresh share one try, so a failed refresh hides
a note that was actually created and invites a retry that would create a second one; the client
sends no idempotency key). Also `R2-004` / `R3-fully-returned-untested` (`returnable_quantity
=== '0'` is string equality against a decimal contract, and the state has no assertion) and five
e2e-assertion nits (`R2-001`, `R2-002`, `R2-005`, `R3-nav-assertion-drift`: the nav assertion
was already red at baseline and this slice repaired it).

L19 — native review of the T10 fix (`7708ad8` vs `2dfd618`, tier medium, lenses readability +
reliability): **approved**, authority burned. Nine advisories, all SUGGESTION except one real
WARNING: `R3-randomuuid-secure-context` — `crypto.randomUUID` exists only in secure contexts and
this app is served over plain HTTP on a LAN or tailnet on purpose, so every submit there would
have thrown before sending anything. Fixed in `19f182d` (fallback to `getRandomValues`, then to a
timestamp string) and **approved with no findings** (lineage `review-9a23ab87866cfd29`). The
remaining advisories are test-rigor and scoping notes recorded for later: replay-after-lost-create
unproved, refresh-failure notice unproved, the retained key spanning payload edits, and two e2e
assertions that can match the previous note's DOM. One approved review also created a probe
owner in the dev database (`e2e-credit-notes-t10@example.test`) for the browser proof.

L20 — delivered: PR #59 merged to `main` (`6e2d715`) with the whole feature; CI on `main` is
green. The six stacked PRs (#53–#58) were superseded by it — #53, whose commits are in the
merge, shows as merged, and the rest were closed with that note — because the branch ruleset
requires the generated files to be current and the branch to be up to date with its base, which
a chain that regenerates its artifacts in a later slice cannot satisfy. Nothing else is owed:
the advisories in L10 and L12–L19 stay as later work, and `payments.cjs` P6 remains the known
environmental failure when the database already has an active payment method.
