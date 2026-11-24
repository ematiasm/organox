# Purchases

## The central decision: the purchase draft lives on the server

Just like `sales`, a purchase in progress does **not** live only in the browser.
It is a row in `purchases` with status `draft`, and its lines live in
`purchase_items`.

Why:

1. **It survives a reload.** Refreshing `/operations/purchases` does not lose a
   long supplier invoice being entered line by line. `POST /purchases/draft`
   reuses the operator's open draft instead of creating a second one.
2. **The total is computed by the server.** Every line mutation (`POST`, `PUT`,
   `DELETE` on `/purchases/{id}/items`) returns the full recalculated purchase.
   The client renders and validates inputs for UX, never as the source of truth.
3. **A draft never touches the stock or money ledgers.** An abandoned or
   cancelled draft leaves no trace in `stock_movements` or `wallet_movements`.

## Unit cost lives on the purchase line and the stock movement, not on the product

Each line in `purchase_items` records `product_id`, `quantity` (`> 0`, up to 3
decimals), and `unit_cost` (`>= 0`, up to 2 decimals). When the purchase
completes, `unit_cost` is copied onto the resulting `stock_movements` row
(`reason = 'purchase'`).

`products` deliberately gains **no** cost column in this version: product cost
tracking and markup-based sale pricing (`cost + %`) are deferred to their own
feature. Keeping `unit_cost` on `purchase_items` and `stock_movements` preserves
exact historical cost per receipt without prematurely coupling the catalog
pricing model.

Zero-cost lines (`unit_cost = 0`) are valid (for example, promotional or bonus
units received from a supplier), so a purchase total can be `"0.00"`; when the
total is zero, completion accepts an empty `payments` list and writes no wallet
movements.

## Goods only

Only active products with `kind = 'product'` (items that track stock) can be
added to or completed in a purchase. Services (`kind = 'service'`) have no stock
ledger and are rejected with `409`. Inactive products are rejected both when
adding lines and again under a row lock at completion.

## Completion: one transaction, stock, wallet, and account together

```
BEGIN
  SELECT status, supplier_id FROM purchases WHERE id = ? FOR UPDATE
  verify status = 'draft', supplier_id IS NOT NULL, items non-empty
  for each item (ordered by product_id):
    SELECT kind, is_active FROM products WHERE id = ? FOR UPDATE
    verify active product (not service); accumulate server-calculated total
  verify Σ payments + on_account == total and active payment methods / wallets
  if on_account > 0:
    authorize_credit:
      pg_advisory_xact_lock(hashtextextended(supplier_id::text, 1)) (5s lock_timeout -> 409)
      SELECT is_active, credit_limit FROM suppliers WHERE id = ? FOR UPDATE
      verify active supplier and balance + on_account <= credit_limit (409 otherwise)
  else:
    SELECT name, is_active FROM suppliers WHERE id = ? FOR UPDATE
    verify supplier is_active
  for each item:
    INSERT stock_movements (reason = 'purchase', quantity, unit_cost)
  for each payment:
    INSERT wallet_movements (kind = 'purchase_payment', purchase_id, payment_method_id)
  UPDATE purchases SET status = 'completed', supplier_name = ?, total = ?, completed_by = ?, completed_at = now(), note = ?
  if on_account > 0:
    INSERT supplier_account_entries (kind = 'purchase_charge', amount = +on_account)
    apply_available_credit oldest-first -> supplier_account_allocations (returns credit_applied)
COMMIT
```

Every item, stock entry, wallet outflow, supplier-account charge, automatic
advance allocation, and status flip commits atomically. If any check fails
(missing or inactive supplier, deactivated product, payment sum mismatch,
inactive method or wallet, credit limit exceeded, or busy supplier account),
the transaction rolls back and leaves the purchase as a `draft` with zero
stock, wallet, or supplier-account movements written.

Because products are locked in deterministic `product_id` order and every
on-account write takes the supplier advisory lock (seed `1`) before
`suppliers FOR UPDATE`, concurrent completions and supplier-account operations
cannot deadlock.

## Supplier snapshot at completion

While a purchase is a `draft`, `PurchaseResponse.supplier_name` reads the live
supplier name (`COALESCE(purchases.supplier_name, suppliers.name)`). At
completion, the same `UPDATE` that flips `status = 'completed'` and freezes
`total` also freezes `supplier_name` onto `purchases`. After that the snapshot
wins, so renaming the supplier later never alters completed purchase history.

The database enforces the invariant with two constraints:
`purchases_supplier_name_needs_its_supplier` and
`purchases_completed_has_supplier`.

## How a purchase is paid

Completion supports upfront wallet payments, buying on account, or a mix of
both:

```json
{
  "payments": [
    { "payment_method_id": "…", "amount": "5000.00" }
  ],
  "on_account": "10000.00",
  "note": "Invoice #1042"
}
```

`on_account` is optional (defaults to `"0.00"`, `>= 0`, at most two decimals),
and the completion invariant is `Σ payments + on_account == purchase.total`.

- **Upfront payments (`payments`)**: multiple rows are supported (for example,
  part cash box and part bank transfer). Each payment writes one
  `wallet_movements` row with `kind = 'purchase_payment'` (sign `-1`, outflow,
  non-manual), carrying both `purchase_id` and `payment_method_id` and copying
  the method's default `wallet_id` at write time (`docs/payments.md`). Wallet
  balances may go negative, matching `expense` movements.
- **On account (`on_account > 0`)**: acquires the supplier advisory lock
  (seed `1`) and locks `suppliers FOR UPDATE` via `authorize_credit`. The
  supplier must be active and `balance + on_account` must fit
  `suppliers.credit_limit` (`NULL` = unlimited, `0` = cash only, `> 0` = cap;
  exceeding it returns `409` naming remaining available credit). In the same
  transaction, completion writes one `purchase_charge` entry (`+on_account`) in
  `supplier_account_entries` and runs `apply_available_credit` to allocate any
  unapplied advance credit (`U`) oldest-first across open payments.
  `CompletedPurchaseResponse.credit_applied` reports how much advance credit
  covered automatically (`"0.00"` when none applied).
- **Cash-only (`on_account == 0`)**: follows the cash path unchanged and writes
  no supplier-account entry.

See `docs/supplier-accounts.md` for the full payables ledger, supplier
payments, credit refunds, adjustments, and payables aging.

## Why a completed purchase cannot be edited or cancelled

`completed` and `cancelled` are terminal states; mutating items, supplier,
cancelling, or re-completing a closed purchase returns `409`. History is
immutable so stock movements and wallet outflows remain auditable.

## API

All routes require a session (`401` without a cookie). Errors use
`application/problem+json` (RFC 7807).

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/purchases/draft` | Opens a draft or reuses the operator's open one (`201` created, `200` reused) |
| GET | `/api/v1/purchases?status=&supplier_id=&limit=&offset=` | Paginated purchases (`{items, limit, offset, total}`), newest first |
| GET | `/api/v1/purchases/{id}` | Full purchase with items, supplier snapshot/live name, and calculated/frozen total |
| PUT | `/api/v1/purchases/{id}/supplier` | Attaches or clears the draft's supplier (`{"supplier_id": "<uuid>"}` or `null`) |
| POST | `/api/v1/purchases/{id}/items` | Adds a line (if already present, adds quantity and updates `unit_cost`) |
| PUT | `/api/v1/purchases/{id}/items/{product_id}` | Sets `quantity` (`> 0`) and `unit_cost` |
| DELETE | `/api/v1/purchases/{id}/items/{product_id}` | Removes the line |
| POST | `/api/v1/purchases/{id}/complete` | Completes the purchase (`payments` + optional `on_account`), writes `stock_movements`, `wallet_movements`, and (when `on_account > 0`) `purchase_charge` + automatic `credit_applied` |
| POST | `/api/v1/purchases/{id}/cancel` | Cancels a `draft` (`204`) |

## What is missing

- **Product cost & markup pricing**: storing a current product cost and deriving
  sale price from `cost + %` is deferred to its own feature.
- **Supplier returns / debit notes**: returning merchandise from a completed
  purchase currently requires manual stock and wallet adjustments until a
  dedicated return document is added.
- **Invoice number / VAT breakdown**: supplier invoice references can be stored
  in `note`, without structured fiscal numbering or tax lines.
- **Orphan drafts**: old abandoned drafts are not cleaned up automatically.
