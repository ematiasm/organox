# Purchases and Suppliers — supplier registry and draft-to-confirm cash purchases

The system currently knows `purchase` only as a manual `stock_movements` reason,
with no supplier registry, no purchase document, and no wallet outflow kind for
supplier payments. This feature adds a `suppliers` registry (mirroring
`customers` without credit limits) and a `purchases` workflow with draft editing,
server-calculated totals, transactional stock entry, and cash wallet outflows.

## Specs

S1. Suppliers registry with full CRUD over the API and admin UI, mirroring `customers` without credit limits (user decision: "Registro de proveedores (Recomendado)"): `name` required; `tax_id` optional and unique when present (case-insensitive `upper(tax_id)`); `email` and `phone` optional free text; `is_active` defaults to `true`.

S2. A supplier is deactivated, never deleted: `POST /suppliers/{id}/deactivate` is idempotent and returns `204`, and a deactivated supplier cannot be attached to a purchase or used to complete one.

S3. Suppliers list uses the paginated `{items, limit, offset, total}` shape with case-insensitive `search` (name or tax_id) and `include_inactive` filter (defaulting to active-only).

S4. Cash-only payments through existing wallets (user decision: "Solo contado con billeteras (Recomendado)"): add `WalletMovementKind::PurchasePayment` (`purchase_payment`, negative sign `-1`, non-manual) in its own enum migration (`0023`), linked via `wallet_movements.purchase_id` (`ON DELETE RESTRICT`) with completeness and link-exclusivity `CHECK` constraints in `0024`. Supplier credit / payable accounts are out of scope.

S5. Purchase document follows the draft-to-confirm lifecycle (user decision: "Borrador → Confirmar (como Ventas)"): statuses `draft`, `completed`, `cancelled`; one open draft per operator (`POST /purchases/draft` returns `{id, reused}`); items and supplier are edited while `draft`; `POST /purchases/{id}/cancel` cancels a `draft` (`204`); completed and cancelled purchases are immutable (`409`).

S6. Each purchase references a supplier (`PUT /purchases/{id}/supplier` while `draft`, atomic `WHERE status = 'draft'`, rejects inactive suppliers). Completion requires an active supplier and freezes `supplier_name` and `total` onto the purchase row so later renames or edits never rewrite history.

S7. Purchase items record `product_id`, `quantity` (`> 0`, up to 3 decimals), and `unit_cost` (`>= 0`, up to 2 decimals) — goods only (`ProductKind::Good`, rejecting `service` and inactive products). Products themselves gain no cost column in this feature (user decision: "los productos empiezan a tener precio costo, el precio de venta va a pasar a ser el calculo del costo + %. Pero lo dejamos para otra feature").

S8. Purchase completion (`POST /purchases/{id}/complete`) runs in one transaction: locks the purchase, verifies `draft`, non-empty items, and active supplier; locks each product, verifies active `good`, and inserts `stock_movements` with `reason = 'purchase'`, `quantity`, and `unit_cost`; validates `payments` sum exactly to the server-calculated `total` (each payment > 0 with up to 2 decimals, or empty list when `total == 0`) against active payment methods and active wallets, and inserts `wallet_movements` with `kind = 'purchase_payment'` and `purchase_id`.

S9. Purchases list (`GET /purchases`) returns paginated `{items, limit, offset, total}` (optional `status` and `supplier_id` filters), and `GET /purchases/{id}` returns the full purchase with items, supplier_id/supplier_name (snapshot-over-join), calculated/frozen total, and timestamps.

S10. Errors follow RFC 7807 `application/problem+json` with English `detail` strings: `409` for duplicate supplier `tax_id`, non-editable purchase, or service/inactive product/supplier violations; `422` for validation failures, empty purchase completion, missing supplier on completion, or payment sum mismatches; `404` for unknown IDs.

S11. Frontend adds `/suppliers` (Catalog rail group, mirroring `/customers`: URL `search`/`page`, show-deactivated toggle, right-side Sheet for create/edit/deactivate) and `/operations/purchases` (Operations rail group: open/resume draft with supplier picker, line editor with quantity and unit cost, split payment confirmation and cancel, plus paginated history with detail drawer).

S12. Every repository artifact is in English (`AGENTS.md`); migrations are forward-only starting at `0022`; `docs/suppliers.md`, `docs/purchases.md`, `docs/payments.md`, and `README.md` are updated; `make prepare`, `make fe-schema`, and `make check` stay clean.

## API

| Method | Path | Result |
|---|---|---|
| `POST` | `/suppliers` | `201 {id}` |
| `GET` | `/suppliers?search=&include_inactive=&limit=&offset=` | `200 {items, limit, offset, total}` |
| `GET` | `/suppliers/{id}` | `200 SupplierResponse` |
| `PATCH` | `/suppliers/{id}` | `204` |
| `POST` | `/suppliers/{id}/deactivate` | `204` |
| `POST` | `/purchases/draft` | `200/201 {id, reused}` |
| `GET` | `/purchases?status=&supplier_id=&limit=&offset=` | `200 {items, limit, offset, total}` |
| `GET` | `/purchases/{id}` | `200 PurchaseResponse` |
| `PUT` | `/purchases/{id}/supplier` | `204` |
| `POST` | `/purchases/{id}/items` | `200 PurchaseResponse` |
| `PUT` | `/purchases/{id}/items/{product_id}` | `200 PurchaseResponse` |
| `DELETE` | `/purchases/{id}/items/{product_id}` | `200 PurchaseResponse` |
| `POST` | `/purchases/{id}/complete` | `200 CompletedPurchaseResponse` |
| `POST` | `/purchases/{id}/cancel` | `204` |

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Backend suppliers module: `0022_suppliers.sql`, `src/suppliers/*`, router, OpenAPI, `tests/suppliers.rs`, `.sqlx`, schema | S1, S2, S3, S10, S12 | inline | done | 2f6f6ae |
| T2 | Wallet `purchase_payment` + backend purchases module: `0023`/`0024` migrations, `src/payments/*` extension, `src/purchases/*`, router, OpenAPI, `tests/purchases.rs`, `.sqlx`, schema | S4, S5, S6, S7, S8, S9, S10, S12 | inline | done | 311ecaa |
| T3 | Frontend suppliers screen: `/suppliers` route, rail item, API wrappers, search/pager/inactive filter, create/edit/deactivate Sheet | S1, S2, S3, S11, S12 | inline | done | 35391d2 |
| T4 | Frontend purchases screen: `/operations/purchases` route, rail item, API wrappers, draft workspace (supplier, items with unit cost, split payments, confirm/cancel), history & detail drawer | S5, S6, S7, S8, S9, S11, S12 | inline | done | 5ae1fab |
| T5 | Documentation & verification: `docs/suppliers.md`, `docs/purchases.md`, `docs/payments.md`, `README.md`, full `make check`, independent verification | S12 | inline + verify | done | 63b9d17 |

## Log

L1. Original user request (verbatim):

> "continua con la creacion de compras"

L2. Scoping decisions taken with the user via `ask_user_question` (verbatim):
- Proveedores: "Registro de proveedores (Recomendado)"
- Pago y crédito: "Solo contado con billeteras (Recomendado)"
- Ciclo de vida: "Borrador → Confirmar (como Ventas)"
- Costo y stock: "los productos empiezan a tener precio costo, el precio de venta va a pasar a ser el calculo del costo + %. Pero lo dejamos para otra feature"

L3. T1 delivered (`2f6f6ae`): `0022_suppliers.sql`, `src/suppliers/*`, router + OpenAPI + `.sqlx` + `schema.d.ts`, `tests/suppliers.rs` (7 integration + 4 validation unit + 5 openapi unit tests green; `fmt`, `clippy -D warnings`, `fe-check` clean).

L4. T2 delivered (`311ecaa`): `0023_wallet_purchase_payment_kind.sql`, `0024_purchases.sql`, `WalletMovementKind::PurchasePayment` + `purchase_id` across `src/payments/*` and callers + payments UI label, `src/purchases/*` (draft-to-confirm lifecycle, atomic stock + wallet writes, frozen `supplier_name`/`total`), OpenAPI + `.sqlx` + `schema.d.ts`, `tests/purchases.rs` (7 integration + 1 unit), `tests/payments.rs` (exhaustive 8-kind sign test + manual rejection test). `fmt`, `clippy -D warnings`, `fe-check` green.

L5. T3 delivered (`35391d2`): `/suppliers` screen (`+page.ts`, `+page.svelte`, `supplier-drawer.svelte`), suppliers API wrappers in `client.ts`, rail link in `app-shell.svelte`; `make fe-check` and `make fe-build` clean.

L6. T4 delivered (`5ae1fab`): `/operations/purchases` screen (`+page.ts`, `+page.svelte`), purchases API wrappers in `client.ts`, rail link in `app-shell.svelte`; `make fe-check` (0 errors, 0 warnings) and `make fe-build` clean.

L7. T5 delivered (`63b9d17`): `docs/suppliers.md`, `docs/purchases.md`, updated `docs/payments.md` and `README.md`; full `make check` green (392 tests passed, `fmt`, `clippy -D warnings`, `svelte-check` 0 errors/0 warnings), `SQLX_OFFLINE=true cargo check --all-targets` green, `make fe-build` green.

L8. Independent verification (`gentle-ai-verify`) caught 4 blockers + 2 UI advisories, all resolved and covered by regression tests: (1) inactive products on `POST /purchases/{id}/items` and `POST /purchases/{id}/complete` now return `409 Conflict` (`PurchaseError::ProductInactive`), (2) `PUT /purchases/{id}/supplier` rejects omitted `supplier_id` (`{}`) with `422`, (3) `docs/purchases.md` and `README.md` aligned with `quantity > 0` on `PUT /purchases/{id}/items/{product_id}`, (4) `list_suppliers` now orders by `lower(s.name), s.id`, plus `/suppliers` filter toggle resets `page: 1` and `/operations/purchases` displays a deactivated draft supplier cleanly. `.sqlx/`, `openapi.json`, and `schema.d.ts` regenerated; 394 tests green.
