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

S7. `search` matches the name or the tax id, case-insensitively; `active` filters and is absent by default so a screen can show both.

S8. Errors are RFC 7807 problem+json with English `detail` strings; the codes are the ones the tree already uses: `409` for a taken tax id, `422` for a validation failure, `404` for an unknown id, `204` for the two mutations that return no body.

S9. The customers screen keeps `search` and `page` in the URL like the products screen, opens a right-side Sheet for create and edit, and renders `problem.detail` as the error text: the server is the only validator.

S10. The point of sale picks a customer on the draft it already opens on load, and the sales history shows the customer name of a completed sale, or a dash when it has none.

S11. Every artifact is English, including migration comments (`AGENTS.md`); migrations are forward-only and the next number is `0013`.

S12. `make check`, `make prepare` and `make fe-schema` stay clean, because CI fails on a stale `.sqlx` cache or a stale generated schema.

## API

| Method | Path | Result |
|---|---|---|
| `POST` | `/customers` | `201 {id}` |
| `GET` | `/customers?search=&active=&limit=&offset=` | `200 {items, limit, offset, total}` |
| `GET` | `/customers/{id}` | `200 CustomerResponse` |
| `PATCH` | `/customers/{id}` | `204` |
| `POST` | `/customers/{id}/deactivate` | `204` |
| `PUT` | `/sales/{id}/customer` | `204` |

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Backend customers module: `0013_customers.sql`, model, validation, repository, service, handlers, router, OpenAPI stubs, `tests/customers.rs` | S1, S2, S3, S6, S7, S8, S11, S12 | inline | pending | — |
| T2 | Sale ↔ customer: `0014_sales_customer.sql`, `PUT /sales/{id}/customer`, snapshot at completion, `SaleResponse.customer_id/customer_name`, `tests/sales.rs` additions, `docs/sales.md` | S4, S5, S8 | writer | pending | — |
| T3 | Customers screen: route `(app)/customers`, list/search/pager, Sheet drawer for create/edit/deactivate, API wrappers, regenerated schema | S1, S2, S9, S12 | writer | pending | — |
| T4 | Point of sale: customer picker on the open draft, customer column in the sales history | S4, S10 | writer | pending | — |
| T5 | `docs/customers.md`, README index and structure tree, final `make check` | S11, S12 | inline | pending | — |

T1 is the foundation and ships first; T2 and T3 touch disjoint surfaces and can
run together once T1 lands; T4 needs both, because the picker consumes T3's list
and writes through T2's route.

## Log

L1. Original user request (verbatim):

> "sigamos con la creacion de clientes"

L2. Starting evidence: there is no customer entity. No `customers` table in `migrations/`, no `customer` column on `sales` (`migrations/0005_sales.sql:17-45`), and the words appear only in prose (`docs/credit-notes.md:102`, `docs/sales.md:100` both list "customer attached to the sale" as missing). The request was therefore scoped as a new feature, not a continuation. Decisions taken with the user one question at a time, quoted as chosen: scope "ABM completo: crear, editar, desactivar, buscar"; sales linkage "Sí, cliente opcional en la venta" (the option described a nullable FK plus a name snapshot so historical sales are not rewritten); fields "Nombre + documento + email + teléfono".

L3. Shape read from the tree before writing any code: catalog is the only paginated list (`src/catalog/handlers.rs:66-76`, `{items,limit,offset,total}`), so `list_sales`-style bare vectors are not the model; the layer split is `repository` (SQL only) / `service` (policy) / `handlers` (HTTP), stated in `src/auth/mod.rs:3-11`; a nullable unique column is a *partial* unique index (`products_sku_unique`, `migrations/0003_catalog.sql:55`); adding a column to `sales` is done additively as in `migrations/0011_credit_note_refund_link.sql:9-24`; the sale is created empty, filled, then completed in two phases (`src/sales/handlers.rs:26-39`), and `total` is frozen at `src/sales/repository.rs:528-534`, which is where the name snapshot belongs.
