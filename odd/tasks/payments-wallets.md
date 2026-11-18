# Payment methods, wallets and their ledger

Payment methods become operator-managed data, each linked to a wallet. Every
collection and every payment writes an append-only movement into that wallet, and
balances are derived. Sale completion records how it was charged, in the same
transaction that deducts stock.

## Specs

S1. Payment methods are data, not code: "voy a necesitar poder crear nuevos
metodos de pagos sin migraciones". A method is a row, created and edited at
runtime; adding one never requires a migration.

S2. Each payment method is linked to a wallet: "cada metodo de pago va a estar
linkeado a una wallet/cuenta que me va a servir para conocer el saldo".

S3. Every collection and every payment impacts a wallet: "cada cobro y pago va a
impactar en la wallet en la que este linkeada el metodo de pago". The wallet is
recorded on the movement itself, so editing a method's wallet later never rewrites
past balances.

S4. Wallet balances are derived: `SUM` over the wallet's movements with the sign
given by the movement kind, never a stored column (decision L2.b, matching how
available stock is derived in `src/sales/repository.rs:356`).

S5. Wallet movements are append-only and follow the stock ledger convention: the
amount is always positive and the kind determines the sign (decision L2.d, matching
`migrations/0003_catalog.sql:63-81` and the trigger at `:99`). Kinds are a Postgres
enum; adding one is code plus one migration line.

S6. `POST /api/v1/sales/{id}/complete` requires a list of payments:
`{"payments": [{"payment_method_id": uuid, "amount": "15000.00"}]}` (decision
L2.a). The service verifies the sum equals the total it computed; a mismatch, an
unknown method or an inactive method is a 422 with a `detail` naming the reason.
An amount with more than two decimals is rejected, not silently rounded (L9.c).
A zero-total sale (a sample, a warranty replacement) has no money to route: the
empty list satisfies `sum = total = 0` and the sale completes with no payment
movement (L9.a).

S7. The payment movements are written inside the existing completion transaction,
after the stock movements and before the commit, so a sale can never be charged
without its money movement (decision L3).

S8. Manual outflows are in scope: `POST /api/v1/wallets/{id}/movements` records an
`expense`, `adjustment_in` or `adjustment_out` with a required positive amount and
an optional note (decision L2.e).

S9. Nothing is deleted. Wallets and payment methods only deactivate (`is_active`),
because the ledger references them with `ON DELETE RESTRICT` and history must stay
readable (S1/S3 traceability).

S10. One implicit currency. No multi-currency, no exchange rate, no currency
column; the `en-US` money formatting of the rest of the app applies unchanged.

S11. Money crosses the HTTP edge as strings with two decimals, and quantities as
normalized strings, exactly as sales already do (`src/sales/handlers.rs:213-219`).

S12. Contract break accepted: `complete` gains a required body, so the frontend
client, `frontend/src/lib/api/schema.d.ts`, the POS screen and the e2e probe change
in this same feature.

S13. Out of scope: credit note or return, cash shift or register close, transfers
between wallets, VAT breakdown, per-wallet statements as files.

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Migration 0008: `wallets`, `payment_methods`, `wallet_movements`, `wallet_movement_kind`, append-only trigger, indexes | S1-S5, S9 | inline | done | 5590e38 |
| T2 | Rust domain and repository: wallet and method CRUD, movement insert, derived balance | S1-S5, S9, S11 | inline | done | 5590e38 |
| T3 | API: `/wallets`, `/payment-methods`, `/wallets/{id}/movements`, OpenAPI + contract test | S1, S2, S8, S10, S11 | inline | done | 5590e38 |
| T4 | `complete` with `payments[]` inside the sale transaction, with the sum and method checks | S6, S7, S12 | inline | done | 22b643a |
| T5 | Frontend: wallets and methods settings, POS payment picker, balances and movements, expense form | S1-S3, S8, S11, S12 | worker | done | b8aaae4 |
| T6 | Docs: `docs/payments.md`, README index, `docs/sales.md` gap list updated | S13 | inline | done | this branch |
| T7 | e2e probe for charging with a method, then full check and close | S6, S12 | inline | blocked | not run yet |

T1 and T2 share one commit: the migration without its repository proves nothing
and the repository cannot be read without the DDL. T1 carries high-risk item 1.

## Log

L1. Original user request (verbatim), answering the direction question with a
correction to the proposed enum design:

> "vamos con metodos de pagos. pero no se si crear enums es lo recomendado porque voy a necesitar poder crear nuevos metodos de pagos sin migraciones. además necesito poder llevar trazabilidad de movimientos de los tipos de pagos y cobros a futuro. es decir, cada metodo de pago va a estar linkeado a una wallet/cuenta que me va a servir para conocer el saldo y cada cobro y pago va a impactar en la wallet en la que este linkeada el metodo de pago."

L2. Decisions taken with the user, one questionnaire:

- L2.a Payments travel as a list from day one: the endpoint validates that the
  amounts sum the server-computed total, and the UI sends a single row. Split
  payments then need no schema change.
- L2.b Balances are derived by `SUM`, never a cached column.
- L2.c The wallet is snapshotted on the movement; `payment_methods.wallet_id` is a
  default, not the read path. Otherwise editing a method re-buckets history.
- L2.d The movement kind is a Postgres enum, following `sale_status` and
  `movement_reason`: a new kind always arrives with new code.
- L2.e Manual outflows are part of this feature, so the balance circuit closes in
  one delivery. Accepted consequence: one large PR, reviewed commit by commit.

L3. Correctness rules stated when the model was proposed, not offered as options:
`wallet_movements` carries the `wallet_id` that received the money at that moment
(L2.c), `sale_id` and `payment_method_id` are nullable because the same ledger
will hold manual movements, and the payment write shares the `tx` that already
wraps stock deduction and the `sales` status update in
`src/sales/repository.rs:294-427`. Charging money without recording where it went
is the failure this feature exists to prevent.

L4. Delivery strategy and slice boundaries. The user chose one PR reviewed
commit by commit (L2.e), so the boundary is per unit, on a single branch. Slice 1
(`5590e38`, T1-T3) measures 2 027 authored lines excluding the `.sqlx` cache
(2 651 with it): a new subsystem whose files are mutually dependent — the
migration is not exercised without the repository, and neither compiles alone —
so splitting it further would produce commits that do not build. Slice 2
(`0ad39f6`, T4) measures 542 lines and carries the contract change plus the
regenerated `openapi.json` and `schema.d.ts`, which belong to it: the generated
files are the published shape of the endpoint. Remaining slices: T5 frontend,
T6 docs, T7 e2e. High-risk item 1 (the migration) lives in slice 1, which is a
commit of its own for review purposes.

L5. Verification so far (backend): `make test` green with 48 unit tests and every
integration suite, including 11 new `tests/payments.rs` cases and 6 new payment
cases in `tests/sales.rs`; `make lint` clean with `-D warnings`; `make prepare`
regenerated. RED was observed for T3 by stashing the routes and the module: every
new payment test failed with 404 against the previous tree, then passed after the
wiring. T4's failure mode was structural (the old signature cannot express the
rule), so its tests were written with the change rather than against it.

L6. Slice 3 (T5, `b8aaae4`) was delegated to a worker because the parent hit the
context backstop, with the backend already committed and green so the contract
could not move underneath it. The worker reported `svelte-check` 0 errors and 0
warnings and no frontend test runner existing; the probe in T7 is what closes
that gap. Its UI decisions, none of which contradict a spec: no method is
preselected, method forms list inactive wallets marked `(inactive)`, deactivation
uses the same two-step confirm as the product drawer, movements load on demand
capped at 50 with a disclosure, and the manual-movement form never offers
`sale_payment`.

L7. Slice 4 (T6, T7 probe) is documentation plus `frontend/e2e/payments.cjs`
(P1-P6). The probe is written and `node --check` clean but **not executed**: it
needs a running app and `E2E_EMAIL`/`E2E_PASSWORD`, which the human holds. T7
stays blocked until that run happens; `make check` (fmt, clippy with `-D
warnings`, every Rust test, `svelte-check`) is green on the whole branch.

L8. Found while touching the POS, outside this feature and not fixed: the
"New sale" button after a charge navigates to `/pos`, but the route is
`/operations/pos` (`frontend/src/routes/(app)/operations/pos/+page.svelte:140`).
Pre-existing, one-word fix, kept out of this diff so the payment change stays
reviewable; it is reported to the human instead.

L9. Independent verification of the high-risk migration (item 1) by
`gentle-ai-verify` against the real binary and a scratch database: all specs
satisfied, all 9 payment routes 401 without a session, `make test` 154 passed.
It also found three things worth fixing, all fixed here:

- L9.a **Blocker, introduced by this feature.** A sale whose total is `0.00` could
  no longer be completed: the empty payment list was rejected and no payment may
  be zero, so a free sample became a dead end (baseline completed it; the DB
  allows `total = 0`, `migrations/0005_sales.sql:36`). Now the sum check is the
  only rule: `0 = 0` passes, and an empty list is still rejected whenever there
  is something to collect.
- L9.b **Advisory, test value.** The S3 test re-pointed a method after a *manual*
  adjustment, so it would have passed even if charges read the method's wallet.
  Replaced by `repointing_a_method_does_not_move_a_recorded_payment`, which
  charges first and then re-points.
- L9.c **Advisory, inconsistency.** Charges and manual movements rounded `10.005`
  to `10.00` and returned success, while the catalog rejects a price with three
  decimals. Both now reject with `"... more than 2 decimal places"`.
- L9.d **Advisory, silent sign.** The balance `CASE` ended in `ELSE -amount`, which
  would have signed any future kind negative. It now names all four kinds with
  `ELSE 0`, and `every_kind_is_counted_with_its_own_sign` fails to compile when a
  kind is added, forcing the query to be checked against it.
