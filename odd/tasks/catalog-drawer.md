# Catalog drawer — view, edit, create and deactivate products from the UI

A right-side drawer over the products list: read a product's data and its stock
movements, edit its fields, create new products and deactivate existing ones.
Frontend only; every endpoint it needs already exists and is tested.

## Specs

S1. "poder hacer click sobre el producto y abra un drawer a la derecha con todos los datos del producto" — clicking a product row opens a right-side `Sheet` with every field of `ProductResponse`: name, SKU, type, price, min stock, current stock (with the low badge), status, description.

S2. "va a estar separado en una parte superior con toda la informacion + un boton 'editar que habilite la edicion de los campos" — the upper half is the data; the Edit button enables those fields as a form saving a partial `PATCH /products/{id}`. The server remains the only validator.

S3. "La parte inferior va a mostar los movimientos de stock de ese preducto" — the lower half lists that product's movements, newest first, read-only, reusing `GET /products/{id}/movements` (the same endpoint the movements screen uses).

S4. The same drawer opens empty from a "New product" button and saves a `POST /products` (user decision: "Sí, mismo drawer").

S5. The drawer includes logical deactivation behind a confirmation, using the idempotent `POST /products/{id}/deactivate` (user decision: "Sí, incluir").

S6. The movements section is read-only in this feature; recording stock from the UI is out of scope (user decision: "Sólo lectura").

S7. No backend change: `GET /products/{id}`, `GET /products/{id}/movements`, `PATCH /products/{id}`, `POST /products` and `POST /products/{id}/deactivate` all exist and are tested. The only client additions are wrapper methods for the mutations.

S8. The drawer is driven by the URL (`/products?id=<uuid>`): Back closes it and a link to a product is shareable, the convention `search` and `page` already follow on this screen.

S9. Decimals travel as strings and are displayed unchanged (ui-admin S10 applies to every field here).

## Tasks

| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | Read-only drawer: Sheet, row click, `?id=`, all fields, movements | S1, S3, S7, S8, S9 | inline | done | c77bc84 |
| T2 | Edit mode: form, `updateProduct`, list refresh after save | S2, S7, S8 | inline | done | this branch |
| T3 | Create mode: "New product" button, `createProduct` | S4, S7 | inline | done | this branch |
| T4 | Deactivate: confirmation, `deactivateProduct`, list refresh | S5, S7 | inline | done | this branch |

T1 is the first PR. T2, T3 and T4 ship together in the second; they are modes of
the same component, so splitting them further would review the same file three
times.

## Log

L1. Original user request (verbatim):

> "vamos con pendientes reales del producto que relevamos al principio: crear/editar productos desde la UI. Mi propuesta es poder hacer click sobre el producto y abra un drawer a la derecha con todos los datos del producto. El drawler va a estar separado en una parte superior con toda la informacion + un boton "editar que habilite la edicion de los campos. La parte inferior va a mostar los movimientos de stock de ese preducto (aca analizamos si va a hacer falta el enpoint en API para reutilizar y en casoi de que si separar en slices). Estas de acuerdo con mi propuesta?"

L2. Scope decisions, taken with the user one at a time: create in the same drawer ("Sí, mismo drawer"); deactivation included ("Sí, incluir"); the movements section read-only ("Sólo lectura"). The endpoint question inside L1 is answered by the tree rather than by new work: no new endpoint is needed, because `GET /products/{id}/movements` already exists and is what the movements screen consumes. Slicing therefore follows the frontend: T1 first, then T2-T4 together. Opened as issue #33 (`type:feature`, `status:approved`).

L3. Slice 2 delivered: edit, create and deactivate over the same drawer. The write-path semantics were read from the server before any form was wired, and every one of them shaped the UI: `PATCH` uses `COALESCE`, so `null` means "unchanged" for every field — the form sends raw strings and omits `price`/`min_stock`/`sku` when their inputs are empty, because the API cannot clear them and pretending otherwise would be a lie the user discovers on reload; `sku` on create is optional and an empty input is omitted; a service cannot carry `min_stock` (the form hides the field and never sends it, matching the server's 422 "a service does not track stock"). The server is the only validator and its `problem.detail` is the error text the form displays — verified live against the four failure cases the form can produce (service with min stock 422, negative price 422, duplicate SKU 409 "the SKU is already in use", `null` price as a no-op 204). Edit mode is transient component state (not shareable, Back closes the drawer); create mode is URL state (`?new`, replacing to `?id=<uuid>` on success so Back leaves the list instead of reopening an empty form). Deactivation uses a two-step inline confirm rather than a dialog — the destructive button stays visible as the price of the action — and closes the drawer after the refresh, because the row disappears from the default list. Reactivation is deliberately absent: the issue scoped deactivation, and `PATCH is_active` exists the day it is wanted.
