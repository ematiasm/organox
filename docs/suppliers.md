# Suppliers

The registry of the businesses and people the shop buys inventory from, and the
one place the purchase ↔ supplier link is documented from the registry's side.
The purchase's side (`docs/purchases.md`) covers how the link behaves during
draft editing and completion.

## Deactivated, never deleted

There is no route that deletes a supplier, and there never will be one without
a fresh decision. The only mutation that takes a supplier out of circulation is
`POST /suppliers/{id}/deactivate` (or `PATCH {"is_active": false}`), which sets
`is_active = false` and answers `204`. Repeated deactivation calls are
idempotent (`204`). Reactivation exists only as a field:
`PATCH {"is_active": true}`; there is no dedicated activate route.

Why: `purchases.supplier_id` points at this row (`0024_purchases.sql`). History
points at a supplier, and a row that disappears breaks it — the same reasoning
that keeps products and customers alive. The foreign key is `ON DELETE RESTRICT`:
the last line of defense if a `DELETE` ever appears.

The accepted cost: a supplier registered by mistake can only be edited or
deactivated. Deactivating does not free the document — the unique index below
matches inactive rows too — but `PATCH` can correct it, so a mistyped document
is fixed rather than worked around with a second row.

## The document is optional, unique, and case-blind

`tax_id` is optional: an informal supplier is still worth registering without a
tax document, so the column is nullable. When present, it is unique on
`upper(tax_id)` through a **partial** index (`WHERE tax_id IS NOT NULL`):

- **Partial** because any number of suppliers may have no document; only two
  with the same one are impossible.
- **`upper(tax_id)`** because a document has no meaningful case: a
  case-sensitive index would let the same supplier in twice by spelling the
  document differently.

An empty string never reaches the database: the service trims it and turns it
into `NULL` before writing, because `""` and `NULL` must not be two ways of
saying "no document" — the partial index treats them differently. The same
normalization applies to `email` and `phone`.

The index name is part of the contract: the repository maps a violation of
`suppliers_tax_id_unique` to the `409` "tax_id is already in use", so renaming
it breaks the error, not just the schema.

## The fields, and why there is no credit limit or payable balance

`name`, `tax_id`, `email`, `phone` — the same core four fields `customers`
started with, and nothing more. Every purchase in this version is paid in full
through existing wallets (`docs/purchases.md`), so suppliers carry no credit
limit and no payable account ledger. When supplier credit is added, it will
arrive with its own ledger and rules rather than speculative columns on this
table.

The optional fields keep the same pragmatic validation rules as `customers`:

- **`email`** requires exactly one `@` with text on both sides and nothing else.
- **`phone`** has no format at all, only a length bound (≤ 40 chars).

## `include_inactive` and `PATCH` semantics

The list shows only active suppliers unless the caller sends
`include_inactive=true`, matching `/products` and `/customers`. Deactivation
hides a supplier from the default listing, never from `GET /suppliers/{id}`, so
historical purchases and detail links keep working.

`PATCH /suppliers/{id}` uses `COALESCE($n, column)`: absent fields (and blank
strings normalized to `None`) leave the stored value untouched. `tax_id`,
`email`, and `phone` can be replaced, not cleared — avoiding partial forms that
accidentally erase fields they did not send.

## The purchase link: draft-only, required at completion

`PUT /purchases/{id}/supplier` attaches or clears a supplier on a draft with
`{"supplier_id": "<uuid>"}` or `{"supplier_id": null}`. The field must be
present (`422` if omitted). The guard lives in `WHERE status = 'draft'` of a
single `UPDATE`, so a closed purchase can never take a new supplier (`409`).
The supplier must exist (`404`) and be active (`409`).

Unlike sales (where a walk-in buyer is anonymous), completing a purchase
**requires** an active supplier: `POST /purchases/{id}/complete` rejects a
missing supplier (`422`) or a supplier deactivated mid-draft (`409`), and
freezes `supplier_name` onto the purchase row in the same `UPDATE` that flips
`status = 'completed'`. After that the snapshot wins, so renaming a supplier
never rewrites historical purchases.

## API

All routes require a session (`401` without a cookie). There is no additional
role gate: registering and editing suppliers is operational work, matching
catalog and customers. Errors use `application/problem+json` (RFC 7807).

| Method | Route | Result |
|---|---|---|
| POST | `/api/v1/suppliers` | `201` with `{id}`; `409` tax_id already in use; `422` invalid input |
| GET | `/api/v1/suppliers?search=&include_inactive=&limit=&offset=` | `200` `{items, limit, offset, total}` |
| GET | `/api/v1/suppliers/{id}` | `200` `SupplierResponse`; `404` unknown id |
| PATCH | `/api/v1/suppliers/{id}` | `204`; `404` unknown id; `409` tax_id taken; `422` invalid input |
| POST | `/api/v1/suppliers/{id}/deactivate` | `204`, idempotent; `404` unknown id |
| PUT | `/api/v1/purchases/{id}/supplier` | `204`; `404` unknown purchase or supplier; `409` closed purchase or inactive supplier; `422` `supplier_id` omitted |

The list is ordered by `lower(name), id`, with `limit` defaulting to 50 and
clamped to 1–200. `total` uses `COUNT(*) OVER ()` with a fallback count when
`offset` is past the end of the result set, and `search` matches `name` or
`tax_id` case-insensitively using the shared `src/search.rs` LIKE escaping.

## Validation rules

| Field | Rule | Where |
|---|---|---|
| `name` | required, trimmed, 1–200 chars | service + `suppliers_name_not_blank` |
| `tax_id` | optional, trimmed, ≤ 32 chars, unique without case when present; empty → `NULL` | service + partial index `suppliers_tax_id_unique` |
| `email` | optional, trimmed, ≤ 254 chars, one `@` with text on both sides | service |
| `phone` | optional, trimmed, ≤ 40 chars, any format | service |
| `email`, `phone`, `tax_id` blank | stored as `NULL`, never `""` | service + `CHECK`s (`*_not_blank`) |

## What is missing

- **Supplier payables / credit accounts**: purchases are cash-only in this
  version; accounts payable, partial payments, and supplier statements are
  deferred to their own feature.
- **Per-supplier purchase summary / catalog**: `GET /purchases?supplier_id=...`
  filters purchases by supplier, but there is no supplier-specific price list or
  lead-time registry.
- **Hard delete**: no route, by decision. A supplier created by mistake is
  deactivated, and its document stays taken until edited.
