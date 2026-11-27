# Customer accounts (credit sales)

How organox tracks what a customer owes, collects against it, applies credit
balances, handles returns on credit sales, and reports receivables.

Related docs: `docs/sales.md` (checkout), `docs/payments.md` (wallets and
money movements), `docs/credit-notes.md` (returns), `docs/customers.md` (the
customer registry and `credit_limit`), `docs/supplier-accounts.md` (the
payables mirror for suppliers).

## The central decision: debt is its own append-only ledger

`customer_account_entries` measures **debt**; `wallet_movements` keeps
measuring **real money**. A wallet movement is written only when cash or bank
money actually enters or leaves a wallet — never when a sale goes on account,
and never when an operator records a manual debt adjustment.

Columns on `customer_account_entries` (`0015_customer_accounts.sql`): `id`,
`customer_id` (`ON DELETE RESTRICT`), `kind` (`customer_account_entry_kind`),
`amount numeric(14,2)`, `sale_id`, `credit_note_id`, `wallet_movement_id`,
`idempotency_key`, `note`, `created_by`, `created_at`. `created_by` carries no
foreign key to `users` for the same reason `wallet_movements.created_by` has
none: deleting a user account must never be able to rewrite or orphan ledger
history.

Unlike `wallet_movements` (where amounts are positive and the kind carries the
sign), `amount` here is **signed**, and the sign is its effect on the balance:

| kind | sign | meaning | required link | forbidden links |
|---|---|---|---|---|
| `sale_charge` | `+` | a sale left part of its total on account | `sale_id` (unique per sale) | `note`, `wallet_movement_id` |
| `adjustment_in` | `+` | manual increase of debt (e.g. opening balance) | non-blank `note` | `sale_id`, `credit_note_id`, `wallet_movement_id` |
| `credit_refund` | `+` | collected credit handed back to the customer | `wallet_movement_id` (unique per movement) | `sale_id`, `credit_note_id`, `note` |
| `receipt` | `−` | money collected against the account | `wallet_movement_id` (unique per movement) | `sale_id`, `credit_note_id`, `note` |
| `credit_note` | `−` | a credit note cancelled part of a sale's debt | `sale_id` + `credit_note_id` (unique per note) | `wallet_movement_id` |
| `adjustment_out` | `−` | manual decrease of debt | non-blank `note` | `sale_id`, `credit_note_id`, `wallet_movement_id` |

Note that `receipt` and `credit_refund` store their operator `note` on the
linked `wallet_movements` row (`customer_account_entries` forbids a duplicate
`note` column on those kinds so the two ledgers cannot drift).

A trigger refuses `UPDATE` and `DELETE` on `customer_account_entries`, matching
`wallet_movements` and `stock_movements`. Neither `customers` nor `sales`
stores a balance or paid flag: every balance and sale outstanding is derived
from the ledger.

## Two derived quantities and one invariant

Let `C` = sum of `sale_charge`, `N` = sum of `|credit_note|`, `Adj` = signed
sum of `adjustment_in` + `adjustment_out`, `R` = sum of `|receipt|`,
`F` = sum of `credit_refund`, and `A` = sum of allocations in
`customer_account_allocations`.

- **Balance**: `COALESCE(SUM(amount), 0)::numeric(14,2)`. Positive means the
  customer owes money; negative means the customer holds credit in their
  favour.
- **Unapplied credit (`U`)**: `R − A − F`, never negative. Money actually
  collected that has neither been allocated to a sale nor refunded back.
- **Outstanding of a sale**: `charge(sale) − allocations(sale) − cancel(sale)`,
  never negative.

Because debt and credit share one signed balance, a customer can owe on one
sale and hold unallocated cash at the same time; `U` is what makes the two
readable separately. Every write preserves:

```text
balance + U == Σ outstanding(sale) + Adj    and    U >= 0
```

## Allocations are per sale and never invented by the server

`customer_account_allocations(id, entry_id, sale_id, amount, created_at)` links
a `receipt` entry to the completed sales of the same customer that it pays
(`UNIQUE (entry_id, sale_id)`, `amount > 0`).

Enforced inside the write transaction and backed by deferred constraint
triggers (`customer_account_allocations_fit_receipt` in `0015`,
`customer_account_allocations_fit_the_sale` in `0016`):

- only a `receipt` entry may own allocations;
- `Σ allocations(entry) <= |entry.amount|`;
- `Σ allocations(sale) <= outstanding(sale)`;
- `Σ allocations + F <= R` per customer (`U >= 0`).

Any portion of a receipt that is not allocated stays as unapplied credit `U` —
never a second entry and never a second table. On `POST /customers/{id}/receipts`,
the server never invents allocations: passing `allocations: []` leaves the
entire collection as available credit. The customer account UI offers an
oldest-first prefill purely as operator convenience; the server still validates
whatever allocations are submitted.

## One lock order, bounded wait

Every balance-changing transaction takes
`pg_advisory_xact_lock(hashtextextended(customer_id::text, 0))` as its
**first** lock, with a local `lock_timeout` of 5 seconds. If another writer
holds the account longer than that, Postgres raises `55P03` and the API answers
`409` ("customer account is busy") so the operator retries instead of queueing
indefinitely.

Operations that also mutate or check the `customers` row (`authorize_credit`
during an on-account checkout, `POST /customers/{id}/deactivate`, and
`PATCH /customers/{id}` with `is_active: false`) take the customer advisory
lock **first** and lock the customer row `FOR UPDATE` **second**. One lock
order prevents deadlocks, stops concurrent collections from over-allocating,
stops concurrent checkouts from breaching a credit limit, and prevents a
deactivation from racing an on-account sale.

## Checkout on account, credit limit and automatic credit

`POST /api/v1/sales/{id}/complete` accepts an optional `on_account` amount
(default `0`, `>= 0`, at most two decimals). The completion invariant is:

```text
Σ wallet payments + on_account == sale.total
```

- `on_account == 0` follows the cash-sale path unchanged (a zero-total sale
  still requires `on_account == 0`).
- `on_account > 0` requires an active customer on the draft. Under the customer
  advisory lock and `customers` row lock, `authorize_credit` checks activity
  and exposure (`balance + on_account` against `customers.credit_limit`).
  `NULL` means unlimited, `0` means cash only, and the upper bound is
  `999999999999.99`. Exceeding the limit rejects the entire checkout with `409`
  naming the remaining credit; a customer holding negative balance spends their
  own credit before the limit applies.
- Inside the same transaction as stock deduction and wallet movements,
  checkout writes one `sale_charge` of `+on_account` (guarded by
  `customer_account_entries_one_charge_per_sale`) and immediately runs
  `apply_available_credit`, allocating from receipts oldest-first
  (`created_at, id`) up to `min(on_account, U, sale_outstanding)`.
- `CompletedSaleResponse.credit_applied` reports how much available credit
  covered automatically; `on_account - credit_applied` is the net debt added.

## Credit notes cancel debt before refunding cash

When `POST /api/v1/sales/{id}/credit-notes` credits an amount `N` against a
sale with `outstanding = O`:

1. `cancel = min(N, O)` writes one `credit_note` entry of `−cancel` in the
   customer's ledger (nothing is written when `cancel == 0`).
2. `cash = N − cancel` is split across the tenders that actually collected
   money for that sale — its original `sale_payment` wallet movements plus the
   receipts allocated to it — each capped at what it collected minus what was
   already refunded. Shares are computed in integer cents using largest
   remainders **cumulatively** across all notes on the sale and subtracted per
   tender, so three notes of `33.33` on a `100.00` sale refund `99.99`, never a
   cent more.

A fully unpaid credit sale therefore writes zero `sale_refund` wallet
movements: no wallet ever collected money for it. See `docs/credit-notes.md`.

## Collections, credit refunds, adjustments and deactivation

- **Collections (`POST /api/v1/customers/{id}/receipts`)**: writes one
  `account_payment` wallet movement (`+1`, carrying `customer_id`,
  `payment_method_id`, and the method's default `wallet_id` at write time) plus
  one `receipt` ledger entry (`-amount`) and its allocations in one
  transaction. Optional `idempotency_key` (max 100 characters; blank keys are
  discarded, non-blank keys keep their exact bytes including padding) makes
  retries safe: fresh writes answer `201`, replays answer `200` with the
  original receipt paired with the account's **current** balance, unapplied
  credit, and allocated sale outstandings at replay time, and reusing a key
  from another entry kind answers `409`.
- **Credit refunds (`POST /api/v1/customers/{id}/credit-refunds`)**: hands
  collected credit back to the customer by writing one `account_payment_refund`
  wallet movement (`-1`, copying the method's default `wallet_id` at write time
  and imposing no wallet-balance floor, matching the rest of `docs/payments.md`)
  plus one `credit_refund` ledger entry (`+amount`) in one transaction.
  Requires a non-blank `note` and is refused with `422` unless `amount <= U`
  and `balance + amount <= 0` — money is only handed back against money
  actually held in the customer's favour. Accepts an optional `idempotency_key`
  with the same `201`/`200`/cross-kind `409` contract and current-position
  replay response.
- **Manual adjustments (`POST /api/v1/customers/{id}/adjustments`)**: records
  `adjustment_in` (`direction: "in"`) or `adjustment_out` (`direction: "out"`)
  with a required non-blank `note`, touching no wallet. Both directions enforce
  `balance >= 0`: an adjustment can load an opening balance or correct a debt,
  never create or destroy available credit. Adjustments intentionally carry no
  `idempotency_key`.
- **Deactivation guard (`POST /api/v1/customers/{id}/deactivate`,
  `PATCH /api/v1/customers/{id}` with `is_active: false`)**: acquires the
  customer advisory lock before the row update and refuses with `409` naming
  the open position while `balance != 0` or `U != 0` (including on an
  already-inactive customer that still carries an open position).

## Read surfaces and receivables aging

- **`GET /api/v1/customers/{id}/account`**: opens a `REPEATABLE READ READ ONLY`
  transaction so customer existence, current position (`balance`,
  `unapplied_credit`, `outstanding_total`), ledger `entries` (oldest first with
  `running_balance`), and `outstanding_sales` (oldest first with `charged`,
  `cancelled`, `allocated`, `outstanding`) all come from one consistent
  snapshot with zero write locks. Entries are paginated by default (`?limit=`
  default 100, clamped to `1..=500`, `?offset=` default 0). `running_balance`
  is a window over the whole ledger, so a later page still reports the balance
  the account really had at that entry, and `entries_total` always counts every
  entry. The account panel opens on the latest page so recent activity is
  visible immediately.
- **`GET /api/v1/customers/{id}/receipts`**: write log of collections, newest
  first (`?limit=`, default 50, clamped to `1..=200`, `?offset=`, default 0),
  joining wallet, payment method, and note from `wallet_movements` and
  batch-loading allocations.
- **`GET /api/v1/customers/{id}/adjustments`**: write log of manual
  adjustments, newest first (`?limit=`, default 50, clamped to `1..=200`,
  `?offset=`, default 0), with signed `amount`.
- **`GET /api/v1/accounts/receivables`**: every customer with `balance != 0` or
  `unapplied_credit != 0`, plus portfolio totals and aging buckets (`0-30`,
  `31-60`, `61-90`, `90+`) computed from each open sale's `completed_at`
  timestamp in whole elapsed days (`FLOOR(EXTRACT(EPOCH FROM (NOW() -
  completed_at)) / 86400)`). There is no due date in this delivery. `items` is
  paginated by default (`?limit=` default 50, clamped to `1..=200`, `?offset=`
  default 0). Totals, aging and `items_total` always describe the whole report,
  never the page.
- **`GET /api/v1/customers` and `GET /api/v1/customers/{id}`**: include derived
  `balance` and `available_credit` alongside `credit_limit`.

## API

All routes require a session (`401` without a cookie) and no additional role
gate: collecting debts, issuing credit refunds, recording adjustments, and
reading statements or receivables are operational work, matching the policy of
catalog, sales, payments, and customers. Errors are `application/problem+json`
(RFC 7807).

| Method | Route | Result |
|---|---|---|
| GET | `/api/v1/customers/{id}/account` | `200` `CustomerAccountResponse` (position, oldest-first entries with running balance and `entries_total`, open sales; `?limit=` default 100, 1..=500, `?offset=` default 0); `404` unknown customer |
| GET | `/api/v1/customers/{id}/receipts?limit=&offset=` | `200` `ReceiptLogResponse[]` (newest first, `limit` 1..=200, `offset` default 0); `404` unknown customer |
| POST | `/api/v1/customers/{id}/receipts` | `201` fresh / `200` replay `ReceiptResponse`; `404` unknown customer or sale; `409` busy account or cross-kind key collision; `422` invalid method, amount, key or allocation |
| POST | `/api/v1/customers/{id}/credit-refunds` | `201` fresh / `200` replay `CreditRefundResponse`; `404` unknown customer; `409` busy account or cross-kind key collision; `422` invalid method, blank note, bad amount or insufficient credit |
| GET | `/api/v1/customers/{id}/adjustments?limit=&offset=` | `200` `AdjustmentLogResponse[]` (newest first, `limit` 1..=200, `offset` default 0); `404` unknown customer |
| POST | `/api/v1/customers/{id}/adjustments` | `201` `AdjustmentResponse`; `404` unknown customer; `409` busy account; `422` blank note, bad amount or balance crossing below zero |
| GET | `/api/v1/accounts/receivables` | `200` `ReceivablesResponse` (portfolio totals, aging buckets, open-position customers, `items_total`; `?limit=` default 50, 1..=200, `?offset=` default 0) |

Checkout (`POST /api/v1/sales/{id}/complete`), credit notes
(`POST /api/v1/sales/{id}/credit-notes`), and customer management
(`GET/PATCH /api/v1/customers`, `POST /api/v1/customers/{id}/deactivate`)
participate in the same ledger and are summarized in their own docs.

## What is missing

- **Due dates and payment terms**: aging buckets count elapsed days from
  `sales.completed_at`; there is no per-customer term or per-sale due date.
- **Interest or late fees**: balances do not accrue interest automatically.
- **Store credit from cash returns**: a credit note's cash surplus above debt
  always refunds the original tenders rather than turning into `U`.
- **Statement export**: statements and receivables are viewed in the UI or via
  JSON, not exported to PDF or CSV.
