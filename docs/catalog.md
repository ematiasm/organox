# Catalog and stock

## The central decision: stock is not stored, it is calculated

`products` **has no** stock column. Stock is the signed sum of
`stock_movements`.

```sql
SELECT SUM(CASE reason
    WHEN 'purchase'   THEN quantity
    WHEN 'adjustment' THEN quantity
    WHEN 'sale'       THEN -quantity
    WHEN 'loss'       THEN -quantity
END)
FROM stock_movements WHERE product_id = $1;
```

Storing `products.stock_actual` as an editable number guarantees that the DB and
the physical warehouse will eventually disagree, and nobody will be able to
explain why. With an append-only ledger, stock can always be reconstructed and
audited, and each unit has a reason and an author.

The accepted cost: "current stock" takes a `GROUP BY` instead of a `SELECT`.
With the `stock_movements (product_id, created_at DESC)` index and tens of
thousands of rows, it is negligible.

## The ledger is immutable, enforced by the DB

`stock_movements` rejects `UPDATE` and `DELETE` with a trigger:

```
ERROR:  stock_movements is append-only: record an adjustment instead of rewriting history
```

An entry error is corrected with an `adjustment` movement, exactly as in a real
warehouse: it leaves a trace of the correction.

A non-obvious corollary: `created_by` **has no foreign key**. The ledger is
immutable, so `ON DELETE SET NULL` would have to touch historical rows when a
user is deleted, and the trigger would reject it. Moreover, a movement's
attribution should not change because the account is later deleted. It is a
soft reference, and that is why it is correct.

## Sign: quantity is always positive

`quantity` has `CHECK (quantity > 0)`. The reason determines the sign, in one
place (`MovementReason::sign`):

| Reason | Effect | Purpose |
|---|---|---|
| `purchase` | `+` | Purchase from a supplier |
| `adjustment` | `+` | Inventory correction |
| `sale` | `−` | Sale to a customer |
| `loss` | `−` | Breakage, expiry, theft |

Storing negative quantities allows "a sale of -5" that adds stock: a sign error
waiting to happen. With positive quantity and a derived sign, that bug is only
representable if someone changes the reason.

## You cannot sell what is not available

Before inserting an outgoing movement, the service reads stock in the same
transaction and rejects it if there is not enough. The error message includes
the available and requested quantities.

```json
{"title":"Conflict","status":409,
 "detail":"insufficient stock: available 2.000, requested 3.000"}
```

To prevent two simultaneous sales of the last unit, the product row is locked
with `SELECT ... FOR UPDATE` before reading stock.

**Known and documented limitation:** the `record_movement_waits_for_product_lock`
test proves that the operation is serialized per product, but it **does not
isolate** the service's `FOR UPDATE`. The `INSERT` into `stock_movements` causes
the foreign key to acquire `FOR KEY SHARE` on the `products` row, and that lock
also conflicts with the test blocker's `FOR UPDATE`. This was verified by
removing `FOR UPDATE`: the test still passed. The correctness of the service's
own lock is not covered by an automated test; the business rule
(`sale_cannot_make_stock_negative`) and the DB `CHECK` are covered.

## Products vs services

One catalog, two types:

| | `product` | `service` |
|---|---|---|
| Stock | calculated | `null` (**not** `0`) |
| `min_stock` | allowed | forbidden (DB `CHECK`) |
| Movements | allowed | rejected by the service |

`stock: null` vs `stock: "0"` is not a stylistic detail: "not applicable" and
"zero units" are different, and the client needs to distinguish them to avoid
showing "out of stock" for a delivery service.

A service cannot receive movements either: the service layer checks this, backed
by the fact that `kind` is read inside the transaction.

## Deactivate, never delete

A product with movements is not deleted (`ON DELETE RESTRICT`). It is
deactivated: `is_active = false`, it disappears from default listings, and its
history remains intact. A product without history could be deleted, but the
only exposed operation is `deactivate`—less surface area, and an accidental
catalog deletion cannot be undone by definition.

## Money and quantities in JSON: strings, not numbers

Every decimal travels as a **string**:

```json
{"price": "1500.50", "stock": "12.5", "quantity": "12.5"}
```

An `f64` in JSON loses precision for money and fractional quantities. The client
parses to decimal (`decimal.js`, `Decimal` in Go, `BigDecimal` in Java).

Format by type:
- **money** (`price`, `unit_cost`): always 2 decimal places → `"1500.50"`
- **quantities** (`stock`, `quantity`, `min_stock`): no trailing zeros → `"12.5"`

The column is `numeric(12,3)`, so `SUM` returns `"12.500"` without normalization;
applying it at the boundary avoids leaking a storage detail into the HTTP
contract.

## API

All routes require a session (401 without a cookie). Writing the catalog is
operational work: `owner` and `manager` can do it.

| Method | Route | Description |
|---|---|---|
| GET | `/api/v1/products` | List with calculated stock. Filters: `kind`, `search`, `include_inactive`, `limit`, `offset` |
| POST | `/api/v1/products` | Create |
| GET | `/api/v1/products/{id}` | Details with stock |
| PATCH | `/api/v1/products/{id}` | Updates only the fields supplied |
| POST | `/api/v1/products/{id}/deactivate` | Soft-deactivate, idempotent |
| GET | `/api/v1/products/{id}/movements` | History, newest first |
| POST | `/api/v1/products/{id}/movements` | Records a movement |

`limit` is capped at 200: a `limit` of 10 million is a DoS by query.

Example of creation and stock entry:

```bash
curl -b jar.txt -X POST localhost:3000/api/v1/products \
  -H 'content-type: application/json' \
  -d '{"kind":"product","name":"Flour 000","sku":"HAR-001","price":"1500.50","min_stock":"10"}'

curl -b jar.txt -X POST localhost:3000/api/v1/products/<id>/movements \
  -H 'content-type: application/json' \
  -d '{"reason":"purchase","quantity":"50","unit_cost":"1200"}'
```

## Validation rules

| Field | Rule | Where |
|---|---|---|
| `name` | nonempty, ≤ 200 chars, trimmed | service |
| `sku` | ≤ 64 chars, unique when present, `""` → `NULL` | service + partial index |
| `price` | ≥ 0, ≤ 2 decimal places | service |
| `quantity` | > 0, ≤ 3 decimal places | service + `CHECK` |
| `note` | truncated to 500 chars (not rejected) | service |

An empty SKU is normalized to `NULL` so `""` and `NULL` are not two ways to say
"no SKU": the unique index is partial (`WHERE sku IS NOT NULL`), and the two
values would behave differently.

## What is missing

- **Editing a movement's reason**: unavailable by design. Correct it with an
  adjustment.
- **Inventory valuation report**: `unit_cost` exists per movement, so the data is
  there; the query and the decision on which cost to use (last, weighted average,
  FIFO) are missing.
- **Low stock alerts**: `low_stock` is calculated and exposed, but there is no
  notification.
- **Bulk catalog import**.
