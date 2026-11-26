# Read paths: batching and pagination (#109)

Issue #109, delivered as one PR with three work-unit commits: sales reads,
account statements and receivables/payables, then payments and the ledger logs.

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

T1 | S1, S2 | inline | U1: batch `sales::list_completed` with `ANY($1)` and paginate `GET /sales` with a `{items, limit, offset, total}` envelope; update the sales screen.
T2 | S2 | inline | U2: accept bounded `limit`/`offset` on customer and supplier statements plus receivables/payables, reporting the full count; totals, aging, and the position stay computed over the whole account.
T3 | S2, S3 | inline | U3: check wallet existence with `SELECT 1` instead of the full ledger read, and accept `offset` on wallet movement and account log endpoints.
T4 | S2 | inline | Regenerate the OpenAPI spec and TS types, update the docs tables, and run the full local checks.

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
