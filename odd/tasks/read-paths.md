# Read paths: batching and pagination (#109)

Issue #109, delivered as one PR with three planned work-unit commits (sales reads,
account statements and receivables/payables, payments and ledger logs) plus
follow-up fixes from review.

## Specs

S1. `sales::list_completed` batches items and payments with `ANY($1)` in O(1)
queries (matching `credit_notes::load_notes`).

S2. Sales, account statements, receivables/payables, and ledger list endpoints
accept bounded pagination parameters.

S3. `payments::list_movements` checks wallet existence with a lightweight
`EXISTS` query.

S4. Delivery: "Un PR con 3 commits de unidad".

S5. Scope: "In: Query batching and pagination for the affected list endpoints
(plus OpenAPI schema regeneration). Out: Changing ledger balance calculation
semantics."

## Tasks

T1 | S1, S2 | inline | U1 (commit eb58eb7): batch `sales::list_completed` with `ANY($1)` and paginate `GET /sales` with a `{items, limit, offset, total}` envelope; update the sales screen.
T2 | S2 | inline | U2 (commit b66a82e): accept bounded `limit`/`offset` on customer and supplier statements plus receivables/payables, reporting the full count; totals, aging, and the position stay computed over the whole account.
T3 | S2, S3 | inline | U3 (commit 09e3732): check wallet existence with `SELECT 1` instead of the full ledger read, and accept `offset` on wallet movement and account log endpoints.
T4 | S2 | inline | Follow-up commits `01aed7e` (OpenAPI `parameter_in = Query` + contract test), `794dbfd` (log offset clamp and terminal-page tests), and PR #136 review fixes (bounded fallback, read snapshot on receivables, service-layer clamping, unconstrained portfolio sum casts, frontend/OpenAPI polish).

## Log

L1. "segui con el proximo issue" — continue with the next open issue after #108: #109.

L2. Decision: one PR, three work-unit commits, chosen by the human from three
delivery options.

L3. Decision: statement `entries` and receivables/payables `items` pagination is
opt-in. Omitting `limit` keeps today's complete result, so the account panels
keep pairing charges with their credit notes and the running balance stays
continuous; passing `limit`/`offset` returns a bounded window plus the full
count (`entries_total` / `items_total`). Totals, aging, and the account position
are always computed over the whole account (S5).

L4. Decision: log endpoints (`/receipts`, `/adjustments`, `/payments`,
`/wallets/{id}/movements`) keep their bare array and gain `offset` next to the
existing bounded `limit`; a fifth envelope is not invented for them.

L5. Slice boundaries (one PR, three work units, generated files travel with
the unit that changes them): U1 ends at `eb58eb7` (sales), U2 at `b66a82e`
(statements and portfolio reports, customer and supplier), U3 closes the
feature with the wallet existence probe and the log offsets.

L6. The supplier half of U2 mirrors the customer half function for function:
`list_entries_in` + `count_entries_in`, `payables_page` + the past-the-end
re-read, and the same two query structs in the handlers. A later change to one
side has to change the other.

L7. Decision (human, superseding L3): statements and portfolio reports are
bounded by default — `limit` defaults to 50 (clamped 1..=200) on receivables
and payables, and 100 (clamped 1..=500) on customer and supplier statements.
The `/receivables` and `/payables` screens paginate with `?page=`, and both
account panels paginate their ledgers starting on the latest page so recent
activity is always shown first.
