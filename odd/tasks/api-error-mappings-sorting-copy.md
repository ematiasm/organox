# API error mappings, case-insensitive sorting, and copy accuracy (#117)

Objective: Fix the seven low-severity review findings gathered in GitHub issue #117 — a wrong 401/403 mapping, case-sensitive customer name ordering, an unmapped unique-constraint violation in the single-allocation write path, a wrong repository error message, raw decimal formatting in a 409, a hardcoded rejection message for eight movement kinds, and `200 []` for an unknown product's movements.
Branch: `fix/117-api-error-mappings-sorting-copy` | Delivery: pending user decision | Runner: `cargo test` (`make migrate`, `make prepare`, `make check`)

## Specs

S1. `AuthError::Forbidden` maps to HTTP `403 Forbidden` (`src/auth/handlers.rs`, `tests/auth.rs`).
  - Issue quote: "1. `AuthError::Forbidden` maps to HTTP `401 Unauthorized` instead of `403 Forbidden` at `src/auth/handlers.rs:285` (currently masked because handlers call `user.require_user_management()?` first)."
  - `From<AuthError> for AppError` splits the shared arm: `InvalidCredentials` keeps `AppError::Unauthorized` (401), `Forbidden` becomes `AppError::Forbidden` (403).

S2. Customer names sort case-insensitively (`src/customers/repository.rs`, `src/customer_accounts/repository.rs`, `tests/customers.rs`, `tests/account_reads.rs`).
  - Issue quote: "2. `customers/repository.rs:113, 145` and `customer_accounts/repository.rs:585` sort case-sensitively by `c.name` (ignoring `customers_name_lower_idx`), while `suppliers` sorts by `lower(name)`."
  - Both `customers` list `ORDER BY` clauses and the `customer_accounts` receivables window order by `lower(name)`, matching `migrations/0013_customers.sql:42` `customers_name_lower_idx`, the same idiom `src/suppliers/repository.rs:99,131` and `src/supplier_accounts/repository.rs:675` already use.

S3. `customer_accounts::repository::allocate` maps `customer_account_allocations_once_per_sale` to `DuplicateAllocation` (`src/customer_accounts/repository.rs`, `tests/customer_accounts.rs`).
  - Issue quote: "3. `customer_accounts::repository::allocate` (`src/customer_accounts/repository.rs:1220-1231`) does not map unique constraint `customer_account_allocations_once_per_sale` to `DuplicateAllocation` (falls through to `500`)."
  - The INSERT maps the constraint through the existing `violated_constraint` helper (`src/customer_accounts/repository.rs:742`), the same way `record_receipt` maps its two constraints.

S4. `catalog::repository::RepoError::SkuTaken` says the SKU is in use (`src/catalog/repository.rs`).
  - Issue quote: "4. `catalog::repository::RepoError::SkuTaken` (`src/catalog/repository.rs:20-21`) has error text `\"the email is already registered\"`."
  - The message becomes `"the SKU is already in use"`, the same text the public error at `src/catalog/service.rs:23` and the 409 at `src/catalog/handlers.rs:320` already use.

S5. The 409 insufficient-stock message prints normalized quantities (`src/catalog/handlers.rs`, `tests/catalog.rs`).
  - Issue quote: "5. `catalog/handlers.rs:323-328` formats raw `{available}`/`{requested}` decimals in `409` messages (leaking `\"12.500\"` instead of normalized quantities)."
  - The handler formats both through the existing `format_quantity` (`src/catalog/handlers.rs:272`), so the body reads `insufficient stock: available 12.5, requested 20` and matches `CatalogError::InsufficientStock`'s own `Display`.

S6. Rejecting a non-manual wallet movement names that kind's own recording path (`src/payments/model.rs`, `src/payments/service.rs`, `tests/payments.rs`).
  - Issue quote: "6. `payments/service.rs:202` hardcodes `\"a sale payment is recorded by charging a sale, not by hand\"` for all 8 non-manual movement kinds."
  - `WalletMovementKind` owns the reason each non-manual kind cannot be typed by hand, `is_manual()` is derived from that one table (single source of truth), and the service returns the reason for the kind actually sent.

S7. `GET /api/v1/products/{id}/movements` returns `404` for an unknown product (`src/catalog/service.rs`, `src/catalog/handlers.rs`, `tests/catalog.rs`, `docs/catalog.md`).
  - Issue quote: "7. `GET /api/v1/products/{id}/movements` (`src/catalog/service.rs:279-288`) returns `200 []` for an unknown product ID instead of `404`."
  - `list_movements` proves the product exists first with a primary-key probe and returns `CatalogError::NotFound`, the shape `payments::service::list_movements` already uses for the same reason; the `#[utoipa::path]` stub documents the new 404.

## Tasks

- [x] T1 | S1 | inline | commit `848e61a`: split the shared arm so `Forbidden` becomes `AppError::Forbidden` (403) while `InvalidCredentials` stays 401. Unit test on the conversion, RED observed (`left: 401, right: 403`) before the fix.
- [x] T2 | S2 | inline | commit `630b3a7`: order the two `customers` list clauses and the customer receivables window by `lower(name)`. Two tests, both RED observed (`["Alpha","aardvark","beta"]` and `["Banana","apple"]`).
- [x] T3 | S3 | inline | commit `39306b3`: map `customer_account_allocations_once_per_sale` and its supplier twin through `violated_constraint` to `DuplicateAllocation`. Two repository-level tests, both RED observed (`Err(Database(...23505...))`) before the fix.
- [x] T4 | S4-S5 | inline | commit `03fbdb3`: `SkuTaken` reads `the SKU is already in use`; the 409 formats both quantities through `format_quantity`. Unit test and HTTP test, RED observed (`"the email is already registered"`, `available 12.500`).
- [x] T5 | S7 | inline | commit `03fbdb3`: `list_movements` proves the product exists with a new primary-key probe (`repository::product_exists`) and returns `404`; the `#[utoipa::path]` stub documents the response and `make fe-schema` regenerated `openapi.json` and `frontend/src/lib/api/schema.d.ts`. HTTP test, RED observed (`200` for an unknown id).
- [x] T6 | S6 | inline | commit `e3d163d`: `WalletMovementKind::manual_entry_rejection` owns one reason per non-manual kind and `is_manual` is derived from it; the service returns the reason for the kind actually sent. HTTP test, RED observed (`"a sale payment is recorded by charging a sale..."` for `purchase_refund`).
- [x] T7 | S2, S6 | worker | Both review advisories closed, and both new tests proven to bite: reverting the CTE clause to `c.name` fails the ordering test with `["Banana","Date"]`, and corrupting one phrase fails the table test naming the drift.
- [x] T8 | S7 | inline | Second-review advisory closed: one test pins both surviving branches of the existence gate — a product that never moved answers `200 []` and a deactivated product still answers `200` with its ledger. Both proven to bite by narrowing `product_exists` (`is_active` only, then `is_active` + a non-empty-history `EXISTS`).

## Log

L1. User request (2026-11-28): "encara el proximo issue" — the next open issue in the queue is GitHub issue #117 (`api: fix minor error mappings, case-insensitive customer sort, and copy inaccuracies`), the lowest-numbered open issue after #109–#116 closed in ascending order.
L2. Discovery (2026-11-28): read-only explorer mapped all seven findings to `path:line`. Two corrections to the issue text: the receivables `ORDER BY` is at `src/customer_accounts/repository.rs:690` (not 585), and `SkuTaken`'s repository text is currently unreachable through HTTP because `src/catalog/service.rs:23` re-exports its own message.
L3. Finding (2026-11-28): `customer_accounts::repository::allocate` (`:1271`) is not reachable through HTTP (production allocation goes through the batched `apply_available_credit` at `:1399`), but it is public and directly exercised by repository-level tests (`tests/customer_accounts.rs:1625`, `:1958`, `:2917`, `:3052`), so S3 is testable exactly like the existing cases. `supplier_accounts::repository::allocate` (`:1167`) carries the same unmapped `INSERT ... .await?` for `supplier_account_allocations`; the issue's "consistently across domains" and the supplier twin's identical shape justify fixing both in T3.

L4. Verification (2026-11-28): every task observed RED before its fix and GREEN after. `cargo fmt` clean, `SQLX_OFFLINE=true cargo clippy --all-targets -- -D warnings` clean, full `SQLX_OFFLINE=true cargo test` green across all 29 binaries (117 unit tests + every integration crate), `make fe-schema` regenerated `openapi.json` and `frontend/src/lib/api/schema.d.ts`, and `npm run check` reports 0 errors and 0 warnings. Differential check not applicable: the only shared code touched is `From<AuthError> for AppError` (named by the request) and the `is_manual` predicate now derived from the new table; both are pinned by the suite that already covers the sibling behavior (`tests/auth.rs` 401/403 cases, the `src/payments/model.rs` unit tests for all 11 kinds).
L5. Slice boundary (2026-11-28): 5 work-unit commits, 449 authored lines including tests — slightly past the ~400-line slice, so the delivery shape is the maintainer's call (one PR for this feature branch, or split). Nothing pushed.
L6. Assessment (2026-11-28): native assess over `f3d224f..HEAD` (`committedOnly`) reports native risk `high` (hot paths `src/auth/handlers.rs`, `src/payments/model.rs`) and `reviewDue: true`; writer self-verification stands and no escalation was claimed.

L7. Advisories closed in the same PR, commit `test(customers,payments): pin the CTE ordering and every rejection phrase` (2026-11-28): the maintainer chose to close both native-review advisories here rather than in a follow-up. T7 adds no production code: `tests/customers.rs` now seeds `apple`/`Banana`/`cherry`/`Date` and asserts both the full order and `limit: 2` page membership at offsets 0 and 2, and `src/payments/model.rs` pins all eleven `manual_entry_rejection` outcomes with one table-driven unit test.
L8. Teeth proof (2026-11-28): both new tests were proven to fail before being accepted. Reverting the CTE clause to `c.name` (plus a cache regeneration) fails the ordering test with `left: ["Banana", "Date"]` instead of `["apple", "Banana"]`; corrupting one phrase in the production arm fails the table test with `left: Some("a purchase refund is recorded by a SUPPLIER RETURN, not by hand")`. Both mutations reverted; `src/customers/repository.rs` back to its committed content and `src/payments/model.rs` carrying only the new test. `cargo fmt --check`, clippy `-D warnings` and the full `cargo test` suite green again afterwards, and CI on the PR head reports Format and lints, Tests, Frontend, Generated files, CodeQL and the three Analyze jobs all passing.

L9. Third advisory closed (2026-11-28): the maintainer asked for it in a new commit. `http_movements_of_an_existing_product_stay_200` pins the two branches the fix left unobserved — a product that exists and never moved answers `200 []`, and a deactivated product still answers `200` with its ledger, because the probe is a bare primary-key lookup and the ledger outlives the product.
L10. Teeth proof for T8 (2026-11-28): narrowing `product_exists` to `WHERE id = $1 AND is_active AND EXISTS (SELECT 1 FROM stock_movements ...)` fails the empty-history assertion (`left: 404, right: 200` at `tests/catalog.rs:689`); narrowing it to `AND is_active` alone fails the deactivated assertion (`left: 404, right: 200` at `tests/catalog.rs:706`). Both mutations reverted and the cache regenerated; `src/catalog/repository.rs` is back to its committed content. First attempt at this proof silently did nothing (the perl pattern ate `$1`, so the mutated run re-tested unmutated code and passed) — the proof was redone with a python exact-count assertion instead.
