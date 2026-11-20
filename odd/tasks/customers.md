# Customers — a first-class registry, and the sale that points at one

The system knows no customer today: sales are anonymous, credit notes refund a
sale without naming who brought it back, and `docs/sales.md:100` and
`docs/credit-notes.md:102` both list the gap. This feature adds a `customers`
registry with create, edit, deactivate, search and a screen, and lets a sale
optionally reference one.

## Specs

S1. Full ABM over the API and the admin UI: create, edit, deactivate, search (user decision: "ABM completo: crear, editar, desactivar, buscar").

S2. Fields, and only these (user decision: "Nombre + documento + email + teléfono"): `name` required; `tax_id` optional and unique when present; `email` and `phone` optional free text.

S3. A customer is deactivated, never deleted: `is_active` defaults to true, `POST /customers/{id}/deactivate` is idempotent and returns `204`, and a deactivated customer cannot be attached to a sale.

S4. A sale points at a customer optionally (user decision: "Sí, cliente opcional en la venta"): `PUT /sales/{id}/customer` with `{customer_id}` or `{customer_id: null}`, draft-only, `204`.

S5. The customer name is frozen onto the sale at completion, because a completed sale is a fact: renaming a customer afterwards must not change what a past sale says. The snapshot is written in the same transaction that flips `status='completed'` and freezes `total`.

S6. The list response is the catalog shape — `{items, limit, offset, total}` — because it is the only paginated analogue in the tree; `GET /auth`-style bare vectors are not copied.

S7. `search` matches the name or the tax id, case-insensitively. The list shows active customers only; `include_inactive` widens it, the same flag the products list takes, because the screen offers "show deactivated" rather than "show only deactivated".

S8. Errors are RFC 7807 problem+json with English `detail` strings; the codes are the ones the tree already uses: `409` for a taken tax id, `422` for a validation failure, `404` for an unknown id, `204` for the two mutations that return no body.

S9. The customers screen keeps `search` and `page` in the URL like the products screen, opens a right-side Sheet for create and edit, and renders `problem.detail` as the error text: the server is the only validator.

S10. The point of sale picks a customer on the draft it already opens on load, and the sales history shows the customer name of a completed sale, or a dash when it has none.

S11. Every artifact is English, including migration comments (`AGENTS.md`); migrations are forward-only and the next number is `0013`.

S12. `make check`, `make prepare` and `make fe-schema` stay clean, because CI fails on a stale `.sqlx` cache or a stale generated schema.

## API

| Method | Path | Result |
|---|---|---|
| `POST` | `/customers` | `201 {id}` |
| `GET` | `/customers?search=&include_inactive=&limit=&offset=` | `200 {items, limit, offset, total}` |
| `GET` | `/customers/{id}` | `200 CustomerResponse` |
| `PATCH` | `/customers/{id}` | `204` |
| `POST` | `/customers/{id}/deactivate` | `204` |
| `PUT` | `/sales/{id}/customer` | `204` |

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Backend customers module: `0013_customers.sql`, model, validation, repository, service, handlers, router, OpenAPI stubs, `tests/customers.rs` | S1, S2, S3, S6, S7, S8, S11, S12 | inline | done | 1b52e99 |
| T2 | Sale ↔ customer: `0014_sales_customer.sql`, `PUT /sales/{id}/customer`, snapshot at completion, `SaleResponse.customer_id/customer_name`, `tests/sales.rs` additions, `docs/sales.md` | S4, S5, S8 | writer | done | 8f812c7 |
| T3 | Customers screen: route `(app)/customers`, list/search/pager, Sheet drawer for create/edit/deactivate, API wrappers, regenerated schema | S1, S2, S9, S12 | writer | done | this branch |
| T4 | Point of sale: customer picker on the open draft, customer column in the sales history | S4, S10 | writer | done | fe8c7d3 |
| T5 | `docs/customers.md`, README index and structure tree, final `make check` | S11, S12 | writer | done | this branch |

T1 is the foundation and ships first; T2 and T3 touch disjoint surfaces but run
in order, not together: T2 changes `SaleResponse`, so it regenerates
`openapi.json` and `schema.d.ts`, and two writers regenerating the same
committed artifact at once would collide. T4 needs both, because the picker
consumes T3's list and writes through T2's route.

Slice boundary: T1 is one commit of 2,653 changed lines, of which 1,127 are
generated (`.sqlx`, `openapi.json`, `schema.d.ts`). The remaining ~1,526 are a
new domain layer (976 lines of source), its 482 lines of tests and this
document. It is not split: `CONTRIBUTING.md` requires code, tests and docs in
one commit, and half a module has no behaviour to review.

## Log

L1. Original user request (verbatim):

> "sigamos con la creacion de clientes"

L2. Starting evidence: there is no customer entity. No `customers` table in `migrations/`, no `customer` column on `sales` (`migrations/0005_sales.sql:17-45`), and the words appear only in prose (`docs/credit-notes.md:102`, `docs/sales.md:100` both list "customer attached to the sale" as missing). The request was therefore scoped as a new feature, not a continuation. Decisions taken with the user one question at a time, quoted as chosen: scope "ABM completo: crear, editar, desactivar, buscar"; sales linkage "Sí, cliente opcional en la venta" (the option described a nullable FK plus a name snapshot so historical sales are not rewritten); fields "Nombre + documento + email + teléfono".

L3. Shape read from the tree before writing any code: catalog is the only paginated list (`src/catalog/handlers.rs:66-76`, `{items,limit,offset,total}`), so `list_sales`-style bare vectors are not the model; the layer split is `repository` (SQL only) / `service` (policy) / `handlers` (HTTP), stated in `src/auth/mod.rs:3-11`; a nullable unique column is a *partial* unique index (`products_sku_unique`, `migrations/0003_catalog.sql:55`); adding a column to `sales` is done additively as in `migrations/0011_credit_note_refund_link.sql:9-24`; the sale is created empty, filled, then completed in two phases (`src/sales/handlers.rs:26-39`), and `total` is frozen at `src/sales/repository.rs:528-534`, which is where the name snapshot belongs.

L4. T2 delivered: `0014_sales_customer.sql` (nullable `customer_id` with `ON DELETE RESTRICT`, `customer_name` snapshot, `sales_customer_name_needs_its_customer`, partial FK index), `PUT /sales/{id}/customer` reusing `customers::service::get_customer` instead of copying the active rule into sales SQL, the snapshot written in the same `UPDATE` that flips `status` and freezes `total`, and `SaleResponse.customer_id/customer_name` with snapshot-over-join precedence. Nine tests, `make check` green.

Parent correction during review of the slice: `set_customer` first read the status and then wrote, so a sale completed between the two statements could receive a customer and stay without a snapshot — a closed sale whose name could still change. The guard moved into the `WHERE status = 'draft'` of a single `UPDATE`, and the reason is in the code. The item mutations keep their read-then-write: they branch on the status, this one does not.

Independent verification (Risk: item 3 — the completion statement is shared with the POS checkout, the wallet ledger and the credit-note flow) returned PASS on S4, S5 and S8, with the full suite green on both the candidate and the base tree and the atomic attach proven by lock choreography out of tree. Its one advisory was real and is fixed: an omitted `customer_id` was read as "clear the customer", contradicting the documented contract, so presence is now required (`Some(None)` vs `None` via a deserializer) and a cleared customer is an explicit `null`. A test pins the 422 and that the stored row is untouched.

Recorded boundaries, not fixed here because they are older than this feature: malformed request bodies get axum's `text/plain` rejection instead of problem+json on every route in the tree, and `sales_customer_id_idx` has no query yet — it is there for per-customer history, which T4 or a later slice will read.

L5. T3 delivered by a writer: `(app)/customers` with `?search`, 1-based `?page`, `?include_inactive` and `?id`/`?new`, a Sheet drawer with create, partial edit and a two-step deactivate, five `request<T>` wrappers, a Customers entry in the Catalog nav group and `frontend/e2e/customers.cjs`. No frontend unit runner exists in this repo, so the runnable surface was the probe itself: it ran against `make run` on :3000 with the dev database and the freshly built app, **14/14 checks**, after the first run failed 12/14 on two probe bugs (a row click blocked by the drawer overlay, and the deliberately triggered 409 counted as a console error). `npm run check` 0 errors / 0 warnings and `npm run build` clean.

The two server rules this screen may not paper over are written in `client.ts` and in the drawer: an empty optional input travels as `undefined`, never `''`, and `PATCH` cannot clear a field, so no affordance offers it. Every failure is displayed from `problem.detail`, including the duplicate document's 409.

L6. T4 delivered by a writer: a customer picker on the draft the POS opens on load (server-side search by name or document, a readable "No customer" clear option, the selected name beside the ticket total, and a refusal that shows `problem.detail` while keeping the previous customer), a Customer column in the sales history, `setSaleCustomer` in `client.ts`, and `frontend/e2e/pos-customer.cjs` with 22 checks. The writer chose a new probe over extending `sales.cjs` because that file's rule labels belong to the credit-notes task.

The probe proved the feature and missed a defect at the same time: the history row gained a cell and the header did not, so every label sat one column to the left. A parent review of the diff caught it, a check was added for exactly that (`thead` cell count and the Customer label), and the fix was observed twice: 22/23 with the misaligned header from the pre-fix build, 23/23 after rebuilding. That is the useful shape of the lesson — the probe asserted data, not layout, and a column that moves is invisible to a data assertion.

Side effect of running it: `e2e-customers-gate@example.com` (owner, unused password) and the probe's stamped rows remain in the development database, like every probe in this tree leaves its rows.

L7. T5 delivered by a writer: `docs/customers.md` (decisions with their rejected alternatives, the six-route API table, a validation table with the layer that enforces each rule, and an honest missing list), the README docs index, structure tree and API table, plus two corrections the writer found while cross-referencing: `docs/sales.md` said "every item mutation returns the full sale" although `cancel` and the customer attach answer `204`, and `docs/credit-notes.md` still claimed sales were not attached to a customer. The parent closed two README items the writer left out of its scope: the "Current state" section still opened with "Slices 1–3" and the probe list named three probes when seven exist.

Final gate on the branch: `make prepare` and `make fe-schema` regenerate nothing (`git status` shows only documents), `make check` green, 238 tests, svelte-check 0/0, and the two UI probes at 23/23 (`pos-customer.cjs`) and 14/14 (`customers.cjs`) against the built app.
