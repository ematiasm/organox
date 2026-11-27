# Validation consistency gaps (#115)

Objective: Fix the five validation and consistency gaps in GitHub issue #115 across sales quantities, manual wallet movements, catalog pagination echo, supplier return ledger self-defense, and account idempotency key handling.
Branch: `fix/validation-consistency-gaps` | Delivery: `ask-on-risk` (~550 authored changed lines forecasted across 5 work units) | Runner: `cargo test` (`make check`, `make prepare`, `make fe-schema`)

## Specs

S1. Sales draft item quantity (`src/sales/{service,repository,handlers}.rs`).
  - Issue quote: "1. Sales draft item quantity (`src/sales/repository.rs:125-228`): Neither `upsert_item` nor `set_item_quantity` validates 3-decimal scale (`>3` decimals are silently rounded by Postgres), and sending `quantity < 0` to `PUT /api/v1/sales/{id}/items/{product_id}` hits `quantity <= 0` and silently deletes the line instead of returning `422`."
  - `upsert_item` (`POST`) already enforces `quantity > 0`, 3-decimal scale, and `MAX_QUANTITY`.
  - `set_item_quantity` (`PUT`) must reject `quantity < 0` with HTTP `422` (leaving `quantity == 0` to remove the line as used by `remove_item` and pinned by `set_quantity_zero_removes_the_item`), validate 3-decimal scale and `MAX_QUANTITY` for positive quantities, and document `422` in `set_quantity_doc` OpenAPI.

S2. Manual wallet movements on inactive wallets (`src/payments/{service,repository,handlers}.rs`).
  - Issue quote: "2. Manual wallet movements (`src/payments/service.rs:195-256`): `record_manual_movement` does not verify `wallets.is_active`, permitting manual movements on deactivated wallets."
  - Issue quote: "returns `422` for invalid negative/over-precise quantities and inactive wallets"
  - Inside a single transaction (locking the wallet row with `FOR SHARE` / `FOR KEY SHARE` or `FOR UPDATE` to prevent TOCTOU races with concurrent deactivation), verify the wallet exists (`404`) and `wallets.is_active == true` (`422`), then insert the movement.

S3. Catalog pagination echo (`src/catalog/handlers.rs`).
  - Issue quote: "3. Catalog pagination response (`src/catalog/handlers.rs:150-168`): `list_products` echoes the raw unclamped `limit`/`offset` query params instead of the clamped values (`1..=200`, `>= 0`) applied by `catalog::service::list_products`."
  - Clamp `limit` (`1..=200`, default 50) and `offset` (`>= 0`, default 0) before calling the service and echoing them in `ProductListResponse`.

S4. Supplier return ledger self-defense (`src/supplier_accounts/repository.rs`).
  - Issue quote: "4. Supplier return ledger self-defense (`src/supplier_accounts/repository.rs:1326-1375`): Unlike `customer_accounts::record_credit_note` (`src/customer_accounts/repository.rs:1363-1383`), `record_purchase_return` does not verify `purchase_returns.purchase_id == purchase_id` or `amount <= purchase_returns.total`."
  - Mirror `customer_accounts::repository::record_credit_note` in `supplier_accounts::repository::record_purchase_return` (verify `purchase_returns` row exists, `purchase_returns.purchase_id == purchase_id`, and `amount <= purchase_returns.total`) and symmetrically check `purchase_returns` existence, `supplier_id`, and `amount <= total` in `record_purchase_return_credit`.

S5. Idempotency key handling (`customer_accounts`, `supplier_accounts`, `migrations/`, `docs/`).
  - Issue quote: "5. Idempotency key handling (`src/customer_accounts/handlers.rs:339-350` vs `src/credit_notes/service.rs:144-159`): `customer_accounts` silently maps blank keys to `None`, leaves whitespace untrimmed, and lacks a non-blank DB `CHECK` constraint; also clarify/decide whether idempotency replay with a mismatched payload should return `409 Conflict`."
  - User decision 5a (Option B): Unify `customer_accounts` and `supplier_accounts` with `credit_notes` / `purchase_returns` — trim `Some(key)`, reject blank (`""` / whitespace-only) with `422`, reject `chars().count() > 100` with `422`, enforce in the service layer (not only HTTP handlers), and add a DB migration enforcing `CHECK (idempotency_key IS NULL OR btrim(idempotency_key) <> '')` on `customer_account_entries` and `supplier_account_entries`. Update affected tests and docs.
  - User decision 5b (Option A — KISS): Keep key-only replay returning `200 OK` with the original record across all 4 modules (`credit_notes`, `purchase_returns`, `customer_accounts`, `supplier_accounts`), while keeping `409 Conflict` for cross-kind key reuse in accounts. Document this explicitly.

## Tasks

- [x] T1 | S1 | inline | commit `e5c8e98`: Reject `quantity < 0` with `422` in `sales::set_item_quantity`, guard `upsert_item` defensively, document `422` in OpenAPI (`tests/sales.rs` quantity tests 5/5 GREEN after observed RED).
- [x] T2 | S2 | inline | commit `cfcfe01`: Lock wallet `FOR SHARE` and verify `wallets.is_active` inside tx in `payments::record_manual_movement` (`422` on inactive, `404` on missing; `tests/payments.rs` 22/22 GREEN after observed RED).
- [x] T3 | S3 | inline | commit `4500b80`: Echo clamped `limit` (`1..=200`) and `offset` (`>= 0`) in `catalog::handlers::list_products` (`tests/catalog.rs` `http_list_*` 4/4 GREEN after observed RED).
- [x] T4 | S4 | inline | commit `b7ac532`: Add symmetric `purchase_returns` self-defense checks in `supplier_accounts::repository::{record_purchase_return,record_purchase_return_credit}` (`tests/supplier_accounts_ledger.rs` 12/12 + `tests/purchase_returns.rs` 13/13 GREEN after observed RED).
- [x] T5 | S5 | inline | commit `0d5c95d`: Unify `idempotency_key` trimming/blank-422 validation in `customer_accounts` & `supplier_accounts` service layer, add migration `0035_account_idempotency_key_not_blank.sql`, update tests/docs, regenerate `.sqlx/` & OpenAPI, full `cargo fmt --check`, `SQLX_OFFLINE=true cargo check --all-targets`, `SQLX_OFFLINE=true cargo clippy --all-targets -- -D warnings`, `svelte-check`, and full `SQLX_OFFLINE=true cargo test` green.

## Log

L1. User request (2026-11-27): "si, segui con el proximo" (GitHub issue #115: `validation: fix consistency gaps in sales quantities, inactive wallets, catalog pagination echo, and supplier return checks`).
L2. User product decisions for S5 (2026-11-27): "para el primero la opcion B. para 2 la opcion A KISS" — unify blank/trimmed `idempotency_key` validation (trim + 422 on blank + service-layer enforcement + DB CHECK constraint) across `customer_accounts` and `supplier_accounts`; keep KISS key-only replay (`200 OK` with original record, `409` only on cross-kind reuse).
L3. Route & risk assessment (2026-11-27): T1–T3 are <= 100 lines each (inline); T4 and T5 share `src/supplier_accounts/` and `tests/supplier_accounts.rs` so they run sequentially inline. High-risk items present: item 1 (DB migration adding CHECK constraints in T5), item 3 (requested contract change in T1 & T5), item 4 (wallet row locking inside tx in T2). Independent read-only verifier will audit the full feature branch against baseline `c6c917b` at feature end.
L4. Verification evidence (2026-11-27): `make migrate`, `make prepare`, `make fe-schema`, `cargo fmt --check`, `SQLX_OFFLINE=true cargo check --all-targets`, `SQLX_OFFLINE=true cargo clippy --all-targets -- -D warnings`, `cd frontend && npm run check` (0 errors, 0 warnings), and full `SQLX_OFFLINE=true cargo test` all passed on `0d5c95d`. Launching independent read-only verifier against baseline `c6c917b`.
L5. Independent verification (2026-11-27): Fresh read-only verifier audited `c6c917b..0d5c95d` across S1–S5, migration `0035`, contract parity, and `insert_manual_movement` `FOR SHARE` lock ordering, and re-ran `cargo fmt --check`, `SQLX_OFFLINE=true cargo check --all-targets`, `SQLX_OFFLINE=true cargo clippy --all-targets -- -D warnings`, and `SQLX_OFFLINE=true cargo test --lib` (115/115). Verdict: 0 blockers (3 non-blocking advisories noted). Authored changed lines across the 5 code commits: ~919 lines (567 tests, 40 docs, 312 Rust/SQL; generated `.sqlx/`, `openapi.json`, `schema.d.ts` excluded). Ready for delivery decision (`ask-on-risk`: single PR vs stacked PRs).
