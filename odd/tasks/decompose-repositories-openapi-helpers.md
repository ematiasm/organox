# Decompose repositories, attach OpenAPI paths, extract shared helpers (#118)

Objective: Reduce the cognitive load the backend review measured in issue #118 — monolithic repository functions, `#[utoipa::path]` detached from the handlers it documents, and mechanically duplicated helpers — without changing behavior.
Branch: `refactor/118-decompose-repositories-openapi-helpers` | Base: `eac8138` | Delivery: chained PRs (three slices: S1+S2, S3, S4), chosen by the maintainer | Runner: `cargo test` (`make migrate`, `make prepare`, `make check`)

## Specs

S1. `#[utoipa::path]` moves onto the real handlers (`src/*/handlers.rs`, `src/openapi.rs`).
  - Issue quote: "`#[utoipa::path]` is attached directly to handlers (removing the 69 `*_doc()` stubs) with `make fe-schema` producing an identical OpenAPI spec."
  - Acceptance: zero `*_doc()` functions remain, `src/openapi.rs::paths(...)` lists handlers instead of stubs, and `make fe-schema` leaves `openapi.json` and `frontend/src/lib/api/schema.d.ts` byte-identical.
  - Constraint measured: removing a stub while it stays in `paths()` is a compile error, and dropping it from `paths()` while the function stays is a `dead_code` error under `-D warnings`. Each module migrates atomically.

S2. Shared pure helpers move to their one home (`src/money.rs`, `src/http.rs`, `tests/common/mod.rs`).
  - Issue quote: "Pure shared helpers live in `src/money.rs`, `src/http.rs`, and `tests/common/mod.rs` with zero behavioral changes and `make check` green."
  - Issue quote: "`format_money` is duplicated across 10 handler files, `format_quantity` in 4, `present_option` in 4, `max_amount` in 3, `violated_constraint` across multiple repos, and integration test setup (`test_password`, `app`, `seed_user_with_password`) is copy-pasted across 15 test crates."
  - Acceptance: each helper has exactly one definition; `tests/common/mod.rs` exists and every integration crate that needs it imports from there; no behavior changes.

S3. Oversized repository functions decompose into named private phase helpers (`src/*/repository.rs`).
  - Issue quote: "Account repositories are split cleanly into `reads.rs` and `writes.rs`, and 200+ LOC `create`/`complete` functions are decomposed into named private phase helpers."
  - Acceptance: no repository function body above ~100 lines, each phase a named private helper, SQL unchanged, `.sqlx` cache unchanged apart from file moves.

S4. Account repositories split into `reads.rs` and `writes.rs` (`src/customer_accounts/`, `src/supplier_accounts/`).
  - Issue quote: "`customer_accounts/repository.rs` (1,627 LOC) and `supplier_accounts/repository.rs` (1,422 LOC) mix reads, aging reports, locking, and 6 write paths in single files."
  - Out of scope, quoted: "Genericizing `customer_accounts` and `supplier_accounts` SQLx repositories (explicitly rejected under KISS)."

## Current inventory (measured 2026-11-28, not the issue's numbers)

The issue was written against an older base. Measured on `eac8138`:

- **Stubs**: 69 `fn *_doc() {}`, all with `#[allow(dead_code)]`, in 11 handler files (auth 7, catalog 7, customer_accounts 7, customers 5, payments 9, purchase_returns 2, purchases 9, sales 9, supplier_accounts 7, suppliers 5, credit_notes 2). `src/openapi.rs:63-133` lists 69 **function names**. No handler carries an inline `#[utoipa::path]` today, so the convention is uniform, not mixed.
- **Oversized functions**: `credit_notes::create` 339 lines (`:586`), `purchase_returns::create` 379 (`:629`), `sales::complete` 256 (`:702`), `purchases::complete` 223 (`:760`), `receivables_page` 156 (`customer_accounts:583`), `payables_page` 156 (`supplier_accounts:568`), plus `record_receipt` 120, `allocate` 114, `apply_available_credit` 102, `record_credit_refund_with_key` 141, `record_payment` 112, supplier `allocate` 108, `record_credit_refund` 127, `sales::upsert_item` 102.
- **File sizes**: `customer_accounts/repository.rs` 1,875; `supplier_accounts/repository.rs` 1,762. Both directories are flat (`handlers/model/mod/repository/service.rs`); neither has `reads.rs`/`writes.rs`.
- **Helpers, corrected**: `format_money` is **already** extracted to `src/money.rs:55` and the ten handlers only import it — the issue's item is already done. `format_quantity` is 4 byte-identical bodies. `present_option` is **2**, not 4. `max_amount` is 3 one-line bodies that all return `money::MAX_AMOUNT`. `violated_constraint` is 5 byte-identical bodies. `test_password` is in 15 crates, `app` in 14, `seed_user_with_password` in 5 with 2 signature variants. `src/http.rs` does not exist.
- **Guardrails**: no `clippy.toml`, no `[lints]` table, no line-count gate; CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `npm run check`, and a "Generated files are current" job that fails when `make fe-schema` leaves `openapi.json` or `schema.d.ts` dirty. `tests/openapi.rs` asserts structural facts about the in-memory spec (path/tag presence, decimal-as-string, parameter locations); nothing snapshots the spec, so an identical regeneration is the S1 proof.

## Tasks

- [x] T1 | S1 | worker | Migrate `#[utoipa::path]` onto the handlers, delete the 69 stubs, point `openapi.rs::paths(...)` at the handlers, prove `make fe-schema` leaves the generated artifacts identical.
- [x] T2 | S2 | worker | Extract `format_quantity`, `present_option`, `max_amount`, `violated_constraint` to their single homes.
- [x] T3 | S2 | worker | Add `tests/common/mod.rs` and remove the copy-pasted `test_password`/`app`/`seed_user_with_password`.
- [x] T4 | S3 | worker | commit `331df7f`: `credit_notes::create` 339→90 and `purchase_returns::create` 379→86, split into phase helpers in original statement order.
- [x] T5 | S3 | worker | commit `7ffb615`: `sales::complete` 256→42 and `purchases::complete` 223→40, payment-precedence comments moved into the helpers that own them.
- [x] T6 | S3 | worker | commits `c4a669f`, `e4f7304`: `record_receipt` 120→38, `record_credit_refund_with_key` 141→39, `allocate` 114→23 / 115→22, `apply_available_credit` 102→28, `record_payment` 114→38, `record_credit_refund` 131→35. `receivables_page` and `payables_page` stay at 156 lines: each is one declarative query whose macro generates an anonymous row type, so no helper can receive its rows and splitting would only retype the SQL (documented exception, not a miss).
- [x] T7 | S4 | worker | commits `c4a669f`, `e4f7304`: both account repositories are now `repository/{mod,reads,writes}.rs` with the public `repository::` path unchanged; 1,875 → 202/704/1,214 and 1,762 → 185/686/1,100.

## Log

L1. User request (2026-11-28): "ok, hace el merge cuando pase el CI y despues seguimos con el siguiente issue" — PR #150 merged as `eac8138`, issue #117 closed; the next open issue in ascending order is #118.
L2. Discovery (2026-11-28): read-only explorer measured the inventory above before any edit. Three of the issue's own counts are stale (see "Current inventory"), so the specs keep the issue's acceptance wording while the tasks follow the measured tree.
L3. Risk read (2026-11-28): the feature is behavior-preserving by construction, and the safety net is the existing suite (`tests/customer_accounts.rs`, `tests/supplier_accounts_ledger.rs`, `tests/receipts.rs`, `tests/account_operations.rs`, `tests/account_reads.rs`, `tests/openapi.rs`). No high-risk item from the Task Size list applies while SQL text, migrations and the generated spec stay byte-identical; a task that has to change SQL or the spec is a finding, not a refactor.

L4. T1 done (2026-11-28): one worker migrated all 69 attributes onto their handlers module by module, `auth` first as a pilot that passed the spec comparison before the other ten were touched. Every attribute keeps its historical `operationId` through an explicit `operation_id = "<stub>_doc"` pin: utoipa derives the id from the function name, and both generated artifacts publish those ids, so an unpinned move would silently rename 69 wire values. Verified by the parent: `grep '_doc() {}' src/` = 0, 69 attributes, `make fe-schema` diff 0 bytes, fmt and clippy clean, 31 test binaries green. Commit `ee74410`, 12 files, 1,446+/1,621− — a single mechanical move past the line budget on purpose: splitting it per module would put eleven commits each rewriting `openapi.rs`, and a reviewer reads the same 69 transformations either way.
L5. T2 done (2026-11-28): `format_quantity` → `money.rs` (4 copies), `present_option` → new `http.rs` (2), `max_amount` deleted in favour of the existing `money::MAX_AMOUNT` (3 sites), `violated_constraint` → `db.rs` (5). 14 copy definitions removed; no signature, behavior or spec change (`make fe-schema` untouched). Commit `3d1dfb9`, 17 files, 61+/110− (the new `http.rs` included).
L6. T3 done (2026-11-28): `tests/common/mod.rs` holds `test_password` (15 copies), `app` (13) and one three-argument `seed_user_with_password` (5, in two drifted signatures); the two crates that used the two-argument form pass the values their copy hardcoded, and `ratelimit.rs` keeps its `strict_state` router local on purpose. 600 tests before and after. Commit `cae2fce`, 16 files, 131+/284− (the new `tests/common/mod.rs` included).
L7. Slice boundary (2026-11-28): S1 and S2 are done in three commits (3,653 authored lines, mostly mechanical moves and deletions, far past the ~400-line advisory budget). S3 and S4 — decomposing `credit_notes::create` (339), `purchase_returns::create` (379), `sales::complete` (256), `purchases::complete` (223), the two 156-line report pages and the remaining >100-line account functions, then splitting the two 1.8k-line account repositories — are untouched and carry all the remaining risk. Delivery shape stays open for the maintainer.

L8. T4 done (2026-11-28): the two 300-plus-line creates now orchestrate named phase helpers, with every SQL literal and every comment untouched and the conflict arm that owns its own rollback left in the public function. `.sqlx` regenerates to zero diff, all 600 tests pass. Commit `331df7f`.
L9. T5 done (2026-11-28): the two completion paths decomposed the same way; the payment-precedence reasoning (which error wins on the cash path versus the on-account path) moved into the helper that encodes it. The worker added an independent proof on top of the `.sqlx` check — the multiset of SQL string literals in both files is byte-identical to the previous commit. Commit `7ffb615`.
L10. T6 done with one documented exception (2026-11-28): the four long customer-accounts mutations and the three long supplier-accounts mutations are now phase helpers. `receivables_page` and `payables_page` stay at 156 lines because each body is one declarative query whose macro expansion owns an anonymous row type; the only split available retypes the SQL without shortening what a reviewer reads, so it stays. The worker hit the cache trap once while moving a literal (re-indenting it changed the sqlx hash and produced "no cached data"), and documented the indentation in the helper so nobody tidies it back into a break.
L11. T7 done (2026-11-28): both account repositories split into `mod/reads/writes` with the public `repository::` path preserved through re-exports, so no call site outside the module changed. The literal-multiset check added in T5 is now the standing proof for every move: 34/34 and 36/36 literals identical.
L12. Slice boundary (2026-11-28): eight commits, 12,683 authored lines — a refactor this size cannot fit one review slice, which is why the maintainer chose chained PRs: slice 1 = `ee74410..3e9cdea` (S1 openapi + S2 helpers, 4,486 lines of moves), slice 2 = `331df7f..7ffb615` (S3 decomposition), slice 3 = `c4a669f..e4f7304` (S4 file split). Every slice is independently green and independently reviewable; nothing is pushed yet.
