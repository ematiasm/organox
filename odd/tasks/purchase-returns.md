# Purchase returns (supplier returns)

Objective: Support partial and full merchandise returns against completed purchases, returning stock out of inventory, cancelling open supplier-account debt first, and letting the operator choose whether any already-paid surplus is refunded to the original wallets or kept as supplier advance credit.
Branch: `feat/purchase-returns` | Delivery: `ask-on-risk` (forecast: ~1,600 authored lines) | Checks: `make check`, `SQLX_OFFLINE=true cargo check --all-targets`, `make fe-build`, Playwright e2e probe.

## Specs

- **S1 — Immutable return document bound to a completed purchase**:
  - Verbatim user constraint: `"la devolucion nunca puede ser mayor a la compra linkeada. Una devolucion tiene que respetar los productos que ya existen en la compra linkeada. Nunca se puede devolver mas de los productos que figuran en la compra u otro precio, por lo que una devolucion nunca puede ser mayor que una compra por diseño."`
  - Only `completed` purchases can be returned (`409` on `draft`/`cancelled`, `404` on unknown purchase).
  - Returned items must reference `product_id`s present in `purchase_items` (`422` otherwise); `quantity > 0` (max 3 decimals); cumulative returned quantity per product across all returns of the purchase must not exceed `purchase_items.quantity` (`409` naming remaining returnable quantity).
  - Server freezes `unit_cost` from `purchase_items` (client sends only `{product_id, quantity}`), computes `total = (SUM(qty * unit_cost)).round_dp(2)`, and enforces `already_returned_total + total <= purchase.total` (`409` naming remaining returnable money allowance). Zero-cost lines (`unit_cost = 0`) are valid (`total >= 0`).
  - Tables `purchase_returns` and `purchase_return_items` are append-only (DB triggers block `UPDATE`/`DELETE`).

- **S2 — Stock outflow & availability check**:
  - Add `purchase_return` to `movement_reason` (`0030_purchase_return_enums.sql`), sign `-1`, non-manual (rejected by manual `POST /products/{id}/movements`).
  - Update `MovementReason::sign` and all 6 SQL stock-sum `CASE reason` queries (`catalog/service.rs`, `catalog/repository.rs` x4, `sales/repository.rs`).
  - Before writing, lock `products FOR UPDATE` in `product_id ASC` order and check `available >= quantity`; reject the entire return with `409` naming product, available, and requested quantities if stock is insufficient.
  - Insert one `stock_movements` row per returned line with `reason = 'purchase_return'`, positive `quantity`, and frozen `unit_cost`.

- **S3 — Debt cancelled first; operator chooses surplus disposition**:
  - Verbatim user decision: `"El operador elige en cada devolución: reembolso a wallets O dejarlo como saldo a favor en la cuenta del proveedor"`
  - Global lock order matches `purchases::complete`: `purchases FOR UPDATE` -> `products FOR UPDATE (product_id ASC)` -> supplier advisory lock (`hashtextextended(supplier_id::text, 1)`, 5s timeout -> `409`) -> `suppliers FOR UPDATE` (when needed).
  - `debt_cancelled = min(return_total, max(purchase_outstanding, 0))`. If `> 0`, insert `supplier_account_entries` row with `kind = 'purchase_return'`, `amount = -debt_cancelled`, `purchase_id`, `purchase_return_id`.
  - `surplus = return_total - debt_cancelled`. If `surplus > 0`, apply `surplus_mode`:
    - `wallet_refund`: split `surplus` proportionally in integer cents across tenders `(wallet_id, payment_method_id)` that paid this purchase (`purchase_payment` + allocated `payment` wallet movements) using each tender's **remaining refundable capacity** (`paid - prior_purchase_refunded`), skipping zero shares. Insert `wallet_movements` with `kind = 'purchase_refund'` (`+1` inflow, non-manual, `purchase_id`, `payment_method_id`, `purchase_return_id`). If total remaining wallet capacity `< surplus`, reject whole return with `409` naming remaining capacity.
    - `supplier_credit`: require active supplier (`409` if inactive) and insert `supplier_account_entries` row with `kind = 'purchase_return_credit'`, `amount = -surplus`, `purchase_id = NULL`, `purchase_return_id`.

- **S4 — Payables ledger invariants & interoperability**:
  - `outstanding(purchase) = charged('purchase_charge') - cancelled('purchase_return') - allocated` in all 4 `supplier_accounts` queries and deferred DB trigger `supplier_account_allocations_fit_the_purchase`. `OutstandingPurchaseResponse` exposes `cancelled`.
  - `unapplied_credit (U) = SUM(|payment| + |purchase_return_credit|) - SUM(credit_refund) - SUM(allocations)` in `supplier_accounts` (`account_position_in`, `list_payables_in`) and `suppliers` (`list_suppliers`, `find_supplier`).
  - `purchase_return_credit` entries may own allocations (`supplier_account_allocations_are_payments` trigger, `allocate`, oldest-first `apply_available_credit` on future purchases) and can be refunded in cash via existing `POST /suppliers/{id}/credit-refunds`.
  - Identity preserved: `balance + unapplied_credit == outstanding_total + adjustments` and `unapplied_credit >= 0`.

- **S5 — Input validation & idempotency**:
  - Non-empty items without duplicate `product_id`, `quantity > 0` (max 3 decimals), non-blank `reason` (max 500 chars) -> `422`.
  - Optional `idempotency_key` (trimmed; blank or `> 100` chars -> `422`), unique per `(purchase_id, idempotency_key)`: fresh create answers `201 Created`, replay answers `200 OK` with the original return document and writes nothing new (safe under concurrent retries).
  - Any failure rolls back atomically with zero DB writes.

- **S6 — HTTP API & OpenAPI contract**:
  - `POST /api/v1/purchases/{id}/returns` (`201`/`200` `PurchaseReturnResponse`).
  - `GET /api/v1/purchases/{id}/returns` (`200` `PurchaseReturnsSummaryResponse` with `purchase_id`, `purchase_total`, `returned_total`, `returnable_total`, `purchase_outstanding`, `wallet_refundable_capacity`, `items[]`, `returns[]`; `404` unknown purchase).
  - Session required (operational, no extra role gate). Registered in `src/routes.rs`, `src/openapi.rs`, `tests/openapi.rs`; regenerated `openapi.json`, `frontend/src/lib/api/schema.d.ts`, and `.sqlx/`.

- **S7 — Frontend UI**:
  - Update exhaustive TS enum maps for `MovementReason` (`movements`, `product-drawer`), `WalletMovementKind` (`settings/payments`), and `SupplierAccountEntryKind` (`supplier-account-panel`).
  - Add Return action for completed purchases in `/operations/purchases` (table row + detail sheet) linking to `/operations/purchases/{id}/return`.
  - Build `/operations/purchases/[id]/return` mirroring `/operations/sales/[id]/credit-note`, adding live debt-vs-surplus preview and explicit `wallet_refund` vs `supplier_credit` selector when paid surplus `> 0`, plain-HTTP-safe idempotency key, and issued returns history.
  - Verified with `make fe-check`, `make fe-build`, and Playwright e2e probe `frontend/e2e/purchase-returns.cjs`.

- **S8 — Documentation & full verification**:
  - Add `docs/purchase-returns.md`; update `docs/purchases.md`, `docs/supplier-accounts.md`, `docs/payments.md`, `docs/catalog.md`, and `README.md`.
  - Pass `make check`, `SQLX_OFFLINE=true cargo check --all-targets`, `make fe-build`, and independent verification over the high-risk backend slices.

## Tasks

| ID | Task | Specs | Route | Risk | Status | Commit / Evidence |
|---|---|---|---|---|---|---|
| T1 | Migrations `0030`–`0031` + shared ledger extensions (`catalog`, `payments`, `supplier_accounts`, `suppliers`) + unit/DB tests | S1, S2, S3, S4 | inline | high (items 1, 3) | done | `acd6d1b` (117/117 targeted tests, `clippy -D warnings`, offline build) |
| T2 | `src/purchase_returns/` module, routes, OpenAPI, `.sqlx`, `schema.d.ts`, HTTP & concurrency integration tests | S1, S2, S3, S5, S6 | inline | high (items 1, 3) | done | `c2815d8` (91/91 tests, `.sqlx`, `openapi.json`, `schema.d.ts`) |
| T3 | Frontend API client, enum labels, `/operations/purchases` link, `/operations/purchases/[id]/return` page, Playwright e2e probe | S7 | inline | medium | done | `4a3ca3e` (`svelte-check` 0/0, `vite build`, e2e probe 7/7) |
| T4 | Docs (`docs/purchase-returns.md`, updated module docs & `README.md`) + verifier blocker fixes + full verification | S8 | inline | passive + high-risk verify | done | `ada2d81` + docs commit (`make check` 445/445, `SQLX_OFFLINE=true`, `make fe-build`, verifier S1–S8 PASS) |

## Log

- **L1 (2026-11-24)**: User request: `"encaremos las devoluciones a proveedores"`
- **L2 (2026-11-24)**: User clarified return ceiling invariant verbatim: `"la devolucion nunca puede ser mayor a la compra linkeada. Una devolucion tiene que respetar los productos que ya existen en la compra linkeada. Nunca se puede devolver mas de los productos que figuran en la compra u otro precio, por lo que una devolucion nunca puede ser mayor que una compra por diseño."`
- **L3 (2026-11-24)**: User chose paid-surplus disposition verbatim: `"El operador elige en cada devolución: reembolso a wallets O dejarlo como saldo a favor en la cuenta del proveedor"`
- **L4 (2026-11-24)**: Mapped `credit_notes`, `purchases`, `supplier_accounts`, `catalog`, `payments`, OpenAPI, and frontend via read-only `research` subagent. Branch `feat/purchase-returns` created; feature document initialized with 4 inline tasks (T1–T4) and Engram mirror `odd/purchase-returns/tasks`.
- **L5 (2026-11-24)**: T1 committed (`acd6d1b`), T2 committed (`c2815d8`), T3 committed (`4a3ca3e`, Playwright e2e probe 7/7 checks passed with zero console errors).
- **L6 (2026-11-24)**: Independent high-risk verifier audited `main..HEAD` against `S1`–`S8` and reported two blockers: (1) `POST /api/v1/products/{id}/movements` did not reject `MovementReason::PurchaseReturn` as non-manual (S2), and (2) `purchases FOR UPDATE` in `purchase_returns::create` risked an AB-BA deadlock with FK `FOR KEY SHARE` from concurrent `supplier_account_allocations` inserts (S3). Fixed both (`ValidationError::NonManualReason`, `FOR NO KEY UPDATE` on `purchases`), capped `wallet_refundable_capacity` by `returnable_total`, updated stale comments, and added 3 regression tests (manual movement `422`, rounding-drift `409`, concurrent return + payment allocation deadlock proof). Re-ran `make prepare`, `make check` (445/445 passed), `SQLX_OFFLINE=true cargo check --all-targets`, and `make fe-build` — all green; scoped verifier recheck requested.
- **L7 (2026-11-24)**: Independent verifier scoped recheck confirmed both blockers resolved (`S1`–`S8` PASS, zero open blockers). Verifier fixes committed in `ada2d81`; docs and task closure committed on `feat/purchase-returns`. Ready for user publication decision (push / PR).
