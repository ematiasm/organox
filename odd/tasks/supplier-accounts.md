# Supplier accounts — on-account purchases, payables ledger, and supplier credit limits

Mirrors `customer_accounts` for `suppliers`: adds an append-only payables ledger
(`supplier_account_entries` + `supplier_account_allocations`), `suppliers.credit_limit`,
mixed cash + on-account purchase completion with automatic advance application,
supplier payments/refunds/adjustments, deactivation guard, `/payables` aging report,
and supplier drawer + purchases checkout UI.

## Specs

S1. Append-only payables ledger (`supplier_account_entries` and `supplier_account_allocations` in `0025_supplier_accounts.sql`), mirroring `customer_accounts` (user decision: "Espejo completo de customer_accounts (Recomendado)"): signed `amount numeric(14,2)` where positive means we owe the supplier and negative reduces debt / builds unapplied credit (advance). Entry kinds: `purchase_charge` (`+`, unique per `purchase_id`), `payment` (`-`, unique per `wallet_movement_id`), `credit_refund` (`+`, unique per `wallet_movement_id`), `adjustment_in` (`+`, non-blank `note`), `adjustment_out` (`-`, non-blank `note`). No supplier credit-note kind (supplier returns/debit notes remain out of scope). Append-only triggers block `UPDATE`/`DELETE`; deferred triggers enforce `Σ allocations(entry) <= |entry.amount|` and `Σ allocations(purchase) <= charge(purchase)` on completed purchases of the same supplier; `(supplier_id, idempotency_key)` partial unique index guarantees idempotent writes.

S2. Derived position and invariant: `balance = COALESCE(SUM(amount), 0)`, `unapplied_credit = P - A - F >= 0` (where `P = Σ|payment|`, `A = Σ allocations`, `F = Σ credit_refund`), `outstanding(purchase) = charge(purchase) - allocations(purchase) >= 0`, preserving `balance + unapplied_credit == Σ outstanding(purchase) + adjustments` and `unapplied_credit >= 0`. Neither `suppliers` nor `purchases` stores a balance or paid flag.

S3. Wallet movement integration (`0027`, `0028`, `0029`): add `WalletMovementKind::SupplierAccountPayment` (`supplier_account_payment`, sign `-1`, non-manual, outflow when paying a supplier account) and `WalletMovementKind::SupplierAccountRefund` (`supplier_account_refund`, sign `+1`, non-manual, inflow when a supplier refunds unapplied credit/advance), each in its own enum migration; add `wallet_movements.supplier_id` (`ON DELETE RESTRICT`, partial index) with link-only, completeness (requires `supplier_id` + `payment_method_id`), and cross-column exclusivity checks (`sale_id`, `purchase_id`, `customer_id`, `supplier_id`). Manual wallet endpoint rejects both kinds.

S4. Supplier credit limit (`0026_supplier_credit_limit.sql`, user decision: "Incluir credit_limit en proveedores (Recomendado)"): `suppliers.credit_limit numeric(14,2)` (`NULL` = unlimited, `0` = cash-only, `> 0` = cap, `CHECK (credit_limit IS NULL OR credit_limit >= 0)`). Exposed on `POST /suppliers`, `PATCH /suppliers/{id}` (tri-state: omitted = unchanged, `null` = unlimited, value = set), `GET /suppliers`, and `GET /suppliers/{id}`, alongside derived `balance` and `available_credit` (same semantics as `customers`).

S5. Concurrency and deactivation guard: every balance-changing transaction takes `pg_advisory_xact_lock(hashtextextended(supplier_id::text, 1))` first (seed `1`, disjoint from `customer_accounts`'s seed `0`) with `SET LOCAL lock_timeout = '5s'` (`55P03` -> `409` "supplier account is busy"), before locking the `suppliers` row `FOR UPDATE`. Deactivating a supplier (`POST /suppliers/{id}/deactivate` or `PATCH /suppliers/{id}` with `is_active: false`) requires `balance == 0 && unapplied_credit == 0` under that lock, otherwise returns `409 Conflict`.

S6. On-account purchase completion (`POST /purchases/{id}/complete`): accepts optional `on_account` (default `0`, `>= 0`, at most 2 decimals) enforcing `Σ payments + on_account == purchase.total`. When `on_account == 0`, cash path is unchanged. When `on_account > 0`, requires an active supplier, takes the supplier advisory lock first (before/consistent with lock order), runs `authorize_credit` (`NULL` unlimited, `0` cash-only -> `409`, `balance + on_account > credit_limit` -> `409` with available credit, matching `sales`), inserts `purchase_charge`, and auto-applies available `unapplied_credit` oldest-first via `supplier_account_allocations`, returning `credit_applied` in `CompletedPurchaseResponse`.

S7. Supplier accounts HTTP API (session-authenticated, RFC 7807 `application/problem+json`, English `detail` strings):
- `GET /suppliers/{id}/account` — account statement (position, open purchases with age/outstanding, chronological ledger entries with running balance) in a `REPEATABLE READ, READ ONLY` transaction.
- `POST /suppliers/{id}/payments` — records wallet outflow (`supplier_account_payment`) + ledger `payment` + validated explicit `allocations` (never invented server-side; empty `allocations` leaves unapplied credit); idempotency key normalized (blank -> None, <= 100 chars), returns `201` on fresh write and `200` on replay.
- `GET /suppliers/{id}/payments` — payment log (clamped `limit` 1..=200).
- `POST /suppliers/{id}/credit-refunds` — records wallet inflow (`supplier_account_refund`) + ledger `credit_refund`, requires non-blank `note` and `amount <= min(unapplied_credit, -balance)`; idempotent (`201` fresh / `200` replay).
- `POST /suppliers/{id}/adjustments` — records `adjustment_in` or `adjustment_out` (no wallet movement), requires non-blank `note`, and for `adjustment_out` requires `balance >= 0` and `amount <= balance` (`422` otherwise).
- `GET /suppliers/{id}/adjustments` — adjustment log (clamped `limit` 1..=200).
- `GET /accounts/payables` — payables aging report mirroring `GET /accounts/receivables` (totals, 4 aging buckets `0-30`, `31-60`, `61-90`, `>90` days from `purchases.completed_at`, and per-supplier breakdown).

S8. Frontend mirrors `customer_accounts`:
- `/suppliers` displays `balance` and `credit_limit` columns; `supplier-drawer.svelte` adds tri-state `credit_limit` input, balance/available-credit tiles, and embeds `supplier-account-panel.svelte` with `Statement` and `Pay supplier` tabs (oldest-first prefill convenience, allocation validation, client-side idempotency key; refunds & adjustments remain API-only).
- `/operations/purchases` adds on-account checkout controls (amount/full-on-account toggle, credit-limit & available-credit preview, `credit_applied` completion banner), mirroring POS.
- `/payables` page mirroring `/receivables` (KPI cards, aging buckets, open supplier balances table) with rail link in `app-shell.svelte` under Catalog next to Receivables.
- `/settings/payments` excludes the new non-manual kinds from the manual form and maps readable labels for `supplier_account_payment` and `supplier_account_refund`.

S9. Every repository artifact is in English (`AGENTS.md`); migrations are forward-only starting at `0025`; `docs/supplier-accounts.md` is added and `docs/suppliers.md`, `docs/purchases.md`, `docs/payments.md`, `docs/customer-accounts.md`, and `README.md` are updated; `make prepare`, `make fe-schema`, `make check`, `SQLX_OFFLINE=true cargo check --all-targets`, and `make fe-build` pass cleanly.

## API

| Method | Path | Result |
|---|---|---|
| `GET` | `/suppliers/{id}/account` | `200 SupplierAccountResponse` |
| `POST` | `/suppliers/{id}/payments` | `201` (fresh) / `200` (replay) `SupplierPaymentResponse` |
| `GET` | `/suppliers/{id}/payments?limit=` | `200 SupplierPaymentLogResponse` |
| `POST` | `/suppliers/{id}/credit-refunds` | `201` (fresh) / `200` (replay) `SupplierCreditRefundResponse` |
| `POST` | `/suppliers/{id}/adjustments` | `201 SupplierAdjustmentResponse` |
| `GET` | `/suppliers/{id}/adjustments?limit=` | `200 SupplierAdjustmentLogResponse` |
| `GET` | `/accounts/payables` | `200 PayablesResponse` |

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Migrations `0025`–`0029`, wallet `supplier_account_payment`/`supplier_account_refund` + `supplier_id`, core `src/supplier_accounts` model/repository + ledger tests | S1, S2, S3, S4, S5, S9 | inline | done | 404d32d |
| T2 | `suppliers` (`credit_limit`, `balance`/`available_credit`, deactivation guard) + `purchases` on-account completion (`on_account`, `credit_applied`, advisory lock, auto-credit) + tests | S4, S5, S6, S9 | inline | done | 16b14ee |
| T3 | `supplier_accounts` service, HTTP handlers, router, OpenAPI, `.sqlx`, `schema.d.ts`, full HTTP integration tests | S1, S2, S3, S5, S7, S9 | inline | pending | - |
| T4 | Frontend: `client.ts`, `/suppliers` + `supplier-drawer.svelte` + `supplier-account-panel.svelte`, `/operations/purchases` on-account UI, `/payables` + rail, `/settings/payments` labels | S4, S6, S7, S8, S9 | inline | pending | - |
| T5 | Documentation (`docs/supplier-accounts.md`, updated `suppliers.md`, `purchases.md`, `payments.md`, `customer-accounts.md`, `README.md`), full `make check`, independent `gentle-ai-verify` | S9 | inline + verify | pending | - |

## Log

L1. Original user request (verbatim):

> "continuemos con la feat de poder comprar a crédito/cuenta corriente para suppliers. Espejo de clientes"

L2. Scoping decisions taken with the user via `ask_user_question` (verbatim):
- Límite crédito: "Incluir credit_limit en proveedores (Recomendado)"
- Alcance espejo: "Espejo completo de customer_accounts (Recomendado)"

L3. T1 delivered (`404d32d`): migrations `0025`–`0029` (`supplier_account_entries`, `supplier_account_allocations`, `suppliers.credit_limit`, `supplier_account_payment`/`supplier_account_refund` wallet kinds, `wallet_movements.supplier_id` + cross-column exclusivity), `src/payments/*` + callers updated, `src/supplier_accounts/{mod,model,repository}.rs`, `tests/payments.rs` (18 green) and `tests/supplier_accounts_ledger.rs` (4 green), `.sqlx/` prepared, `fmt`/`clippy -D warnings` clean.

L4. T2 delivered (`16b14ee`): `suppliers` gained `credit_limit` (validated, tri-state `PATCH`), derived `balance`/`available_credit`, and zero-position deactivation guard under advisory lock seed `1`; `purchases` gained `on_account` and `credit_applied` on `POST /purchases/{id}/complete` with atomic `409 Conflict` credit-limit checks and oldest-first advance auto-application; 9 new integration tests in `tests/suppliers.rs` and `tests/purchases.rs` (47/47 integration tests + 6 unit tests green), `.sqlx/` refreshed, `clippy -D warnings` and `SQLX_OFFLINE=true cargo check --all-targets` clean.
