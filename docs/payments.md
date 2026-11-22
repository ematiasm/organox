# Payments, wallets and the money ledger

## The idea

Money has a **place**, and every movement says which place it hit.

- A **wallet** is a place money sits: the cash box, the bank account.
- A **payment method** is how a customer pays, and it points at the wallet that
  money lands in by default.
- A **wallet movement** is the fact: this amount entered or left this wallet.

`wallets` and `payment_methods` are data. `wallet_movements` is a ledger.

## Payment methods are rows; movement kinds are an enum

A new payment method — "Mercado Pago", a second bank account — is a row the
operator creates from the settings screen. Making it an enum would mean a
migration, a deploy and a release note every time the business opens an account.

A new movement **kind**, on the other hand, always arrives with code: a writer, a
validation, a screen, a report. `wallet_movement_kind` is therefore an enum, like
`movement_reason`, and adding one is one migration line shipped with that code.

The distinction is business data versus code semantics, not taste.

## The wallet is copied onto every movement

`payment_methods.wallet_id` is the **default** used when charging. It is not the
path used to read history.

If balances were computed by following `method -> wallet`, fixing a
misconfigured method ("this card actually settles into Banco B") would rebucket
every past movement and change last month's balance. The ledger records where the
money actually went, at the moment it went. Re-pointing a method changes where
**future** money lands and nothing else.

## The balance is not a column

```sql
SELECT SUM(CASE kind
    WHEN 'sale_payment'   THEN amount
    WHEN 'account_payment' THEN amount
    WHEN 'adjustment_in'  THEN amount
    ELSE -amount
END)
FROM wallet_movements WHERE wallet_id = $1;
```

Same rule the stock ledger follows. A stored balance drifts the first time a
transaction forgets to update it, and after that nobody can say which number is
true. The index `wallet_movements (wallet_id, created_at DESC)` makes the sum
cheap; a cache is a decision for the day a measurement asks for it.

A balance **can be negative**. An expense is recorded because it happened, not
because the wallet holds enough: refusing to record reality is how the books stop
matching the drawer.

## Amounts are positive; the kind has the sign

`amount` is always positive and the kind decides the direction:

| kind | direction | who writes it |
|---|---|---|
| `sale_payment` | inflow | charging a sale, never by hand |
| `sale_refund` | outflow | issuing a credit note, never by hand |
| `account_payment` | inflow | collecting against a customer's account (`POST /customers/{id}/receipts`), never by hand |
| `expense` | outflow | the operator, from the wallet |
| `adjustment_in` | inflow | the operator, correcting a mistake |
| `adjustment_out` | outflow | the operator, correcting a mistake |

`-500` meaning "money entered" is a sign bug waiting to happen.

## An account payment carries its customer

An `account_payment` is the wallet side of a collection: money a customer
handed over against what their account owes (`POST /customers/{id}/receipts`,
the customers module's route — see `docs/customers.md`). Its movement carries
the `customer_id` it was collected for and the `payment_method_id` used, and
never a `sale_id` or a `credit_note_id`: which sales the money pays is the
receipt's allocations, not the movement's link. The same transaction writes
the movement and the ledger's `receipt` entry, so the wallet never takes money
the ledger refused to book.

The database keeps the attribution honest: only `account_payment` may carry a
customer (`wallet_movements_customer_link_only_for_account_kinds`), and an
`account_payment` without its customer or its method is refused
(`wallet_movements_account_payment_is_complete`). And it is not a manual kind:
`POST /wallets/{id}/movements` refuses it with `422`, because an account
payment written by hand would put money in a wallet with no receipt saying
whose debt it paid.

## The ledger is append-only

A trigger rejects `UPDATE` and `DELETE` on `wallet_movements`, exactly as
`stock_movements` does. A mistake is corrected with an adjustment movement,
which is what an accountant does anyway.

Wallets and methods are never deleted either: `ON DELETE RESTRICT` plus
deactivation keeps the history readable and the balance explainable.

## Charging a sale

`POST /sales/{id}/complete` requires a list of payments:

```json
{ "payments": [ { "payment_method_id": "…", "amount": "15000.00" } ] }
```

A list, not a single method: half cash and half card is an ordinary way to pay.

The server checks that the payments plus the optional `on_account` amount add
up to the total **it** computed, and that every method is active and points at
an active wallet. Then, in the **same transaction** as the stock deduction and
the status update, it writes one `wallet_movement` per payment; the
`on_account` part writes its charge in the customer's account ledger instead.
A sale is never charged without its money record — the till's movements plus
the charge in the customer's ledger — and a rejected completion leaves the
sale as a draft with no stock movement, no money movement and no account
entry.

## Refunding a sale

A charged sale is not reversed or edited: a return is a credit note. Each
refund writes a `sale_refund` movement — an outflow, never written by hand —
that names the sale it reverses, the payment method that collected the money
and the credit note that caused it. Money leaves the wallets that received the
payment, split in proportion to it in cents, with the last payment absorbing
the remainder. The note reports the refunds it produced; see
`docs/credit-notes.md`.

## API

| Method | Route | What it does |
|---|---|---|
| GET | `/api/v1/wallets` | Wallets with their calculated balance |
| POST | `/api/v1/wallets` | Create a wallet |
| GET | `/api/v1/wallets/{id}` | One wallet with its balance |
| PATCH | `/api/v1/wallets/{id}` | Rename or (de)activate |
| GET | `/api/v1/wallets/{id}/movements` | Recent movements, newest first |
| POST | `/api/v1/wallets/{id}/movements` | Record an expense or an adjustment |
| GET | `/api/v1/payment-methods` | Methods with the wallet each one pays into |
| POST | `/api/v1/payment-methods` | Create a method |
| PATCH | `/api/v1/payment-methods/{id}` | Rename, re-point or (de)activate |

`include_inactive=true` is for the settings screen, which must be able to bring a
row back. The POS never asks for inactive rows.

## What is missing

- **Transfers between wallets**: moving cash to the bank. The ledger can express
  it (two movements, one per wallet) but there is no endpoint yet.
- **Fiscal numbering and customer credit balance**: refunds are recorded, but
  there is no numbered fiscal note and no balance in favor of the customer. See
  `docs/credit-notes.md`.
- **Cash shift / register close**: there is no opening count, closing count or
  expected-vs-counted reconciliation per shift.
- **Multi-currency**: one implicit currency, no exchange rates.
- **Statements**: movements are listed, not exported.
