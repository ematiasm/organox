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

Every item mutation returns the **full recalculated sale**. The client does not
sum, does not validate and does not decide: it only renders.

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

When part of the total is on account, the same transaction also authorizes
that part and then writes the charge in the customer's ledger, so the goods,
the money and the debt commit together. The authorization is one gate under
the customer's account lock: the customer must still be active, and the
balance plus the new amount must fit the customer's credit limit. The gate
runs only when something is on account — a customer deactivated mid-draft
does not block a sale the till collects in full — and it holds the customer's
row locked, so a deactivation racing the checkout cannot slip between the gate
and the charge. The charge also serializes against collections on the same
account: a busy account answers `409` and the operator retries, instead of
queueing behind it.

## A service is not sold as an item

`service_cannot_be_sold_as_item`. A service has no stock, so it cannot produce an
outbound movement. If a service ever has to be invoiced, the model changes: it
becomes an item that does not deduct stock, and that needs its own reasoning
about the ledger.

## The customer is optional, and its name freezes at checkout

A sale may name the customer it serves: `PUT /sales/{id}/customer` attaches or
clears one, and only on a draft — a completed or cancelled sale is a fact that
takes no new customer (`409`). The customer must exist (`404`) and be active
(`409`): those checks reuse the customers module's own rule, not a copy in
sales SQL.

`SaleResponse.customer_name` follows the live registry while the sale is a
draft — the sale is still editable, so the name is too. The moment the sale
completes, the same `UPDATE` that flips `status` and freezes `total` also
freezes the name onto `sales.customer_name`. After that the snapshot wins, so
renaming the customer never rewrites what a charged sale says. Same discipline
as `sale_items.unit_price`.

A customer deactivated between the attach and the checkout does **not** block
the sale: the goods already left, and the snapshot is still taken.

The registry itself — its fields, deactivation and what it does not have yet —
is documented in `docs/customers.md`.

## API

| Method | Route | What it does |
|---|---|---|
| POST | `/api/v1/sales/draft` | Opens a draft or returns the open one (`reused: true`) |
| GET | `/api/v1/sales/{id}` | Sale with items and total |
| POST | `/api/v1/sales/{id}/items` | Adds; if it was already there, adds the quantity |
| PUT | `/api/v1/sales/{id}/items/{product_id}` | Sets the quantity; `0` removes it |
| DELETE | `/api/v1/sales/{id}/items/{product_id}` | Removes the item |
| POST | `/api/v1/sales/{id}/complete` | Charges; takes the list of payments and the optional `on_account` part |
| POST | `/api/v1/sales/{id}/cancel` | Cancels the draft |
| PUT | `/api/v1/sales/{id}/customer` | Attaches or clears the draft's customer; body `{"customer_id": "<uuid>"}` or `{"customer_id": null}` |
| GET | `/api/v1/sales` | Most recent charged sales |

## How the sale is paid

Charging requires that the payments plus the optional `on_account` amount add
up to the total the server computed. Each payment writes a movement into the
wallet that method points at, in the same transaction that deducts stock. The
`on_account` part is different: it is the part the customer leaves owing, so it
writes one charge in the customer's account ledger instead of a wallet
movement — still in that same transaction — and it needs an active customer on
the draft whose credit limit still fits it. The limit counts the balance the
customer already carries, so it is a ceiling on total exposure, and it is
refused whole: `409`, the same status the stock rejection uses, with the
remaining credit named in the message — the operator retries with a smaller
on-account part or collects at the till. `NULL` means no limit and `0` means
cash only; what the field means is the registry's rule
(`docs/customers.md`). A sale can therefore be completed with part, or all, of
its total left owed on the account, within the limit; `on_account = 0`
behaves exactly as a cash sale.

When the account takes a part of the total, the customer's available credit
pays it first, automatically: the same completion transaction allocates from
the customer's receipts — oldest first, because credit is not earmarked to
any one receipt — up to the part charged, and the response's `credit_applied`
says how much of it that credit covered. `on_account - credit_applied` is the
debt the sale actually added. A customer holding money in the shop's hands
is never asked for cash again for the same goods, and what the credit did
not need stays usable for the next sale.

Wallets, payment methods and their ledger have their own document:
`docs/payments.md`. The debt ledger, collections, credit refunds, adjustments,
statements and receivables aging are documented in `docs/customer-accounts.md`.

## Why a charged sale cannot be corrected

`completed` is neither edited nor cancelled. Correcting a charged sale is a
**credit note**: its own document, linked to the sale, with its own stock and
wallet movements. Allowing an `UPDATE` on a closed sale would make the stock
ledger and the accounting stop matching, with no trace.

Credit notes are implemented; see `docs/credit-notes.md` for the rules, the
routes and the refund ledger.

## What is missing

- **Cash shift / register close**: there is no shift opening or closing, so cash
  counts are not reconciled against the wallets of `docs/payments.md`.
- **VAT**: the sale stores the total; the tax is not broken out.
- **Mandatory customer**: the customer on a sale is optional
  (`docs/customers.md`; a walk-in buyer is still a sale). Debt, collections,
  credit notes against debt and the per-customer account statement live in
  `docs/customer-accounts.md`, though there is still no per-customer view of
  cash-only sales.
- **Orphan drafts**: there is no cleanup of old drafts.
