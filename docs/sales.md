# Sales

## The central decision: the cart lives on the server

The cart does **not** live in the browser. It is a row in `sales` with status
`draft`, and its items are in `sale_items`.

Why:

1. **It survives a reload.** Refreshing the sale screen does not empty the cart.
   `POST /sales/draft` returns the open draft instead of creating a new one.
2. **It survives a network drop.** The cashier charges when the connection comes
   back.
3. **The total is computed by the server.** A total the browser believes can
   differ from the one charged; the one that counts is the one the API returns.
4. **Stock can be validated on every action**, not only at confirmation.

Every mutation returns the **full recalculated sale**. The client does not sum,
does not validate and does not decide: it only renders.

## A draft never touches the stock ledger

A sale writes `stock_movements` **only at checkout**. An abandoned or cancelled
draft leaves no trace in the ledger.

That is why the `sales` table needs no stock column, and why `cancelled` only
applies to drafts: there is nothing to revert.

## The price is frozen when the item is added

`sale_items.unit_price` copies the product's price at the moment it is added.

If the product goes up in price while the cashier is charging, the cart total
**does not move**. On a completed sale that number is the price charged, and that
is how it stays audited.

## Checkout: one transaction, with the product locked

```
BEGIN
  SELECT status FROM sales WHERE id = ? FOR UPDATE   -- the sale
  for each item:
    SELECT id FROM products WHERE id = ? FOR UPDATE  -- the product
    SUM(movements) → available stock
    if not enough → ROLLBACK, 409
    INSERT stock_movements (reason='sale')
  UPDATE sales SET status='completed', total=?, completed_at=now()
COMMIT
```

Same discipline as `record_movement`. With two registers selling the last unit,
one wins and the other gets `409` with the exact available quantity. Covered by
`two_drafts_competing_for_the_last_units`.

If stock is missing **nothing is written**: no movements, no status, no modified
cart. Covered by `insufficient_stock_leaves_no_state_change`.

## A service is not sold as an item

`service_cannot_be_sold_as_item`. A service has no stock, so it cannot produce an
outbound movement. If a service ever has to be invoiced, the model changes: it
becomes an item that does not deduct stock, and that needs its own reasoning
about the ledger.

## API

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/sales/draft` | Opens a draft or returns the open one (`reused: true`) |
| GET | `/api/v1/sales/{id}` | Sale with items and total |
| POST | `/api/v1/sales/{id}/items` | Adds; if it was already there, adds the quantity |
| PUT | `/api/v1/sales/{id}/items/{product_id}` | Sets the quantity; `0` removes it |
| DELETE | `/api/v1/sales/{id}/items/{product_id}` | Removes the item |
| POST | `/api/v1/sales/{id}/complete` | Charges |
| POST | `/api/v1/sales/{id}/cancel` | Cancels the draft |
| GET | `/api/v1/sales` | Most recent charged sales |

## Why a charged sale cannot be corrected

`completed` is neither edited nor cancelled. Correcting a charged sale is a
**credit note**: a new sale in the opposite direction, with its own stock
movement. Allowing an `UPDATE` on a closed sale would make the stock ledger and
the accounting stop matching, with no trace.

The credit note is not implemented. It is the next step if returns become
necessary.

## What is missing

- **Credit note / return**: does not exist.
- **Payment methods**: today a sale does not record how it was paid (cash, card,
  store credit). It is the biggest gap for operating for real.
- **Cash shift / register close**: there is no shift opening or closing.
- **VAT**: the sale stores the total; the tax is not broken out.
- **Customer attached** to the sale.
- **Orphan drafts**: there is no cleanup of old drafts.
