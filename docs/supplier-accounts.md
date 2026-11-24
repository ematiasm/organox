# Supplier accounts (on-account purchases)

How organox tracks what the business owes a supplier, pays against it, applies
advance credit balances, records credit refunds and manual adjustments, and
reports payables.

Related docs: `docs/purchases.md` (purchase completion), `docs/payments.md`
(wallets and money movements), `docs/suppliers.md` (the supplier registry and
`credit_limit`), `docs/customer-accounts.md` (the receivables mirror for
customers).

## The central decision: payables are their own append-only ledger

`supplier_account_entries` measures **what we owe** (payables);
`wallet_movements` keeps measuring **real money**. A wallet movement is written
only when cash or bank money actually enters or leaves a wallet — never when a
purchase goes on account, and never when an operator records a manual payable
adjustment.

Columns on `supplier_account_entries` (`0025_supplier_accounts.sql`): `id`,
`supplier_id` (`ON DELETE RESTRICT`), `kind` (`supplier_account_entry_kind`),
`amount numeric(14,2)`, `purchase_id` (`ON DELETE RESTRICT`),
`wallet_movement_id` (`ON DELETE RESTRICT`), `idempotency_key`, `note`,
`created_by`, `created_at`. `created_by` carries no foreign key to `users` for
the same reason `customer_account_entries.created_by` and
`wallet_movements.created_by` have none: deleting a user account must never
rewrite or orphan ledger history.

Unlike `wallet_movements` (where amounts are positive and the kind carries the
sign), `amount` here is **signed**, and the sign is its effect on what we owe:

| kind | sign | meaning | required link | forbidden links |
|---|---|---|---|---|
| `purchase_charge` | `+` | a purchase left part of its total on account | `purchase_id` (unique per purchase) | `wallet_movement_id`, `purchase_return_id` |
| `adjustment_in` | `+` | manual increase of payable (e.g. opening balance) | non-blank `note` | `purchase_id`, `wallet_movement_id`, `purchase_return_id` |
| `credit_refund` | `+` | unapplied advance credit refunded back by the supplier | `wallet_movement_id` (unique per movement), non-blank `note` | `purchase_id`, `purchase_return_id` |
| `payment` | `−` | money paid against the supplier's account | `wallet_movement_id` (unique per movement) | `purchase_id`, `purchase_return_id` |
| `purchase_return` | `−` | returned merchandise cancelled open payable on a purchase | `purchase_id`, `purchase_return_id` (unique per return) | `wallet_movement_id` |
| `purchase_return_credit` | `−` | paid surplus from a purchase return kept as unapplied supplier credit | `purchase_return_id` (unique per return) | `purchase_id`, `wallet_movement_id` |
| `adjustment_out` | `−` | manual decrease of payable | non-blank `note` | `purchase_id`, `wallet_movement_id`, `purchase_return_id` |

Purchase returns and return credits (`0030_purchase_return_enums.sql` and
`0031_purchase_returns.sql`) are written exclusively by
`POST /api/v1/purchases/{id}/returns` (see `docs/purchase-returns.md`).

Database constraints in `0025_supplier_accounts.sql` enforce these invariants:
`supplier_account_entries_amount_not_zero`,
`supplier_account_entries_sign_matches_kind`,
`supplier_account_entries_purchase_link`,
`supplier_account_entries_wallet_link`, and
`supplier_account_entries_adjustment_note`, together with partial unique indexes
`supplier_account_entries_one_charge_per_purchase`,
`supplier_account_entries_one_entry_per_movement`, and
`supplier_account_entries_idempotency` on `(supplier_id, idempotency_key) WHERE
idempotency_key IS NOT NULL`.

Triggers (`supplier_account_entries_no_update` and
`supplier_account_allocations_no_update`, backed by
`supplier_accounts_are_append_only()`) refuse `UPDATE` and `DELETE`, matching
`customer_account_entries`, `wallet_movements`, and `stock_movements`. Neither
`suppliers` nor `purchases` stores a balance or paid flag: every balance and
purchase outstanding is derived from the ledger.

## Two derived quantities and one invariant

Let `C` = sum of `purchase_charge`, `R` = sum of `|purchase_return|`, `Adj` =
signed sum of `adjustment_in` + `adjustment_out`, `P` = sum of `|payment| +
|purchase_return_credit|`, `F` = sum of `credit_refund`, and `A` = sum of
allocations in `supplier_account_allocations`.

- **Balance**: `COALESCE(SUM(amount), 0)::numeric(14,2)`. Positive means the
  business owes the supplier; negative means the business holds advance credit
  with the supplier.
- **Unapplied credit (`U`)**: `P − A − F`, never negative. Advance payments and
  `purchase_return_credit` balances that have neither been allocated to a
  purchase nor refunded back.
- **Outstanding of a purchase**: `charge(purchase) − cancelled(purchase) −
  allocations(purchase)`, never negative (backed by deferred triggers
  `supplier_account_allocations_fit_the_purchase` and
  `supplier_account_return_fits_the_purchase`).

Because debt and advance credit share one signed balance, the business can owe
on one purchase while holding unallocated advance money at the same time; `U`
makes the two readable separately. Every write preserves:

```text
balance + U == Σ outstanding(purchase) + Adj    and    U >= 0
```

## Allocations are per purchase and never invented by the server

`supplier_account_allocations(id, entry_id, purchase_id, amount, created_at)`
links a `payment` entry to the completed purchases of the same supplier that it
pays (`supplier_account_allocations_once_per_purchase` `UNIQUE (entry_id,
purchase_id)`, `supplier_account_allocations_amount_positive` `CHECK (amount >
0)`).

Enforced inside the write transaction and backed by triggers in
`0025_supplier_accounts.sql` (`supplier_account_allocations_payment_only`, plus
deferred constraint triggers `supplier_account_allocations_fit_payment` and
`supplier_account_allocations_fit_the_purchase`):

- only a `payment` entry may own allocations, and only against a completed
  purchase of the same supplier;
- `Σ allocations(entry) <= |entry.amount|`;
- `Σ allocations(purchase) <= charge(purchase)`;
- `Σ allocations + F <= P` per supplier (`U >= 0`).

Any portion of a payment that is not allocated stays as unapplied advance credit
`U` — never a second entry and never a second table. On
`POST /api/v1/suppliers/{id}/payments`, the server never invents allocations:
passing `allocations: []` leaves the entire payment as available advance credit.
The supplier drawer UI offers an oldest-first prefill purely as operator
convenience; the server still validates whatever allocations are submitted.

## One lock order, bounded wait

Every balance-changing transaction takes
`pg_advisory_xact_lock(hashtextextended(supplier_id::text, 1))` as its
**first** lock — seed `1`, disjoint from `customer_accounts`'s seed `0` — with
`SET LOCAL lock_timeout = '5s'` (reset to `0` once acquired). If another writer
holds the account longer than 5 seconds, Postgres raises `55P03` and the API
answers `409` ("the supplier account is busy, retry") so the operator retries
instead of queueing indefinitely.

Operations that also check or mutate the `suppliers` row (`authorize_credit`
during an on-account purchase completion, `POST /suppliers/{id}/deactivate`,
and `PATCH /suppliers/{id}` with `is_active: false`) always take the supplier
advisory lock **first** and lock the `suppliers` row `FOR UPDATE` **second**.
One lock order prevents deadlocks, stops concurrent payments from
over-allocating, stops concurrent completions from breaching a credit limit, and
prevents a deactivation from racing an on-account purchase.

## On-account purchase completion, credit limit and automatic credit

`POST /api/v1/purchases/{id}/complete` accepts an optional `on_account` amount
(default `0`, `>= 0`, at most two decimals). The completion invariant is:

```text
Σ wallet payments + on_account == purchase.total
```

- `on_account == 0` follows the cash-purchase path unchanged (a zero-total
  purchase still requires `on_account == 0`).
- `on_account > 0` requires an active supplier on the draft. Under the supplier
  advisory lock (seed `1`) and `suppliers FOR UPDATE`, `authorize_credit` checks
  activity and exposure (`balance + on_account` against
  `suppliers.credit_limit` from `0026_supplier_credit_limit.sql`). `NULL` means
  unlimited, `0` means cash only, and the upper bound is `999999999999.99`.
  Exceeding the limit (or buying on account from a cash-only supplier) rejects
  the entire completion with `409` naming the remaining available credit; when
  the business holds advance credit (`balance < 0`), it spends that advance
  before the limit applies.
- Inside the same transaction as `stock_movements`, `purchase_payment` wallet
  outflows, and freezing `purchases` (`status = 'completed'`, `total`,
  `supplier_name`), completion writes one `purchase_charge` of `+on_account`
  (guarded by `supplier_account_entries_one_charge_per_purchase`) and
  immediately runs `apply_available_credit`, allocating from unallocated
  payments oldest-first (`created_at, id`, capped at `LIMIT 100` unallocated
  payment rows per completion) up to `min(on_account, U, purchase_outstanding)`.
  If a supplier accumulates more than 100 separate partially unallocated
  advance payments, only the oldest 100 are consumed during that completion and
  any remainder stays in `U`.
- `CompletedPurchaseResponse.credit_applied` reports how much advance credit
  covered automatically; `on_account - credit_applied` is the net payable added.

## Supplier payments, credit refunds, adjustments and deactivation

- **Supplier payments (`POST /api/v1/suppliers/{id}/payments`)**: writes one
  `supplier_account_payment` wallet movement (`-1` outflow, carrying
  `supplier_id`, `payment_method_id`, and the method's default `wallet_id` at
  write time, with no wallet-balance floor) plus one `payment` ledger entry
  (`-amount`) and its explicit `allocations` in one transaction. Optional
  `idempotency_key` (max 100 characters; blank keys are discarded, non-blank
  keys keep their exact bytes including padding) makes retries safe: fresh
  writes answer `201`, replays answer `200` with the original payment paired
  with the account's **current** balance, unapplied credit, and allocated
  purchase outstandings at replay time, and reusing a key from another entry
  kind answers `409`.
- **Credit refunds (`POST /api/v1/suppliers/{id}/credit-refunds`)**: receives
  unapplied advance credit back from a supplier by writing one
  `supplier_account_refund` wallet movement (`+1` inflow, copying the method's
  default `wallet_id` at write time) plus one `credit_refund` ledger entry
  (`+amount`) in one transaction. Requires a non-blank `note` and is refused
  with `422` unless `amount <= min(U, -balance)` (equivalently `amount <= U`
  and `balance + amount <= 0`) — a refund is only booked against advance money
  actually held with the supplier. Accepts an optional `idempotency_key` with
  the same `201`/`200`/cross-kind `409` contract and current-position replay
  response.
- **Manual adjustments (`POST /api/v1/suppliers/{id}/adjustments`)**: records
  `adjustment_in` (`direction: "in"`) or `adjustment_out` (`direction: "out"`)
  with a required non-blank `note`, touching no wallet. Enforces
  `balance >= 0` (refusing with `422` when `balance < 0` or when
  `adjustment_out` would push `balance < 0`): an adjustment can load an opening
  payable or correct a payable, never create or consume advance credit.
  Adjustments intentionally carry no `idempotency_key`.
- **Deactivation guard (`POST /api/v1/suppliers/{id}/deactivate`,
  `PATCH /api/v1/suppliers/{id}` with `is_active: false`)**: acquires the
  supplier advisory lock (seed `1`) before the row update and refuses with
  `409` naming the open position while `balance != 0` or `U != 0` (including on
  an already-inactive supplier that still carries an open position).

## Read surfaces and payables aging

- **`GET /api/v1/suppliers/{id}/account`**: opens a `REPEATABLE READ, READ
  ONLY` transaction so supplier existence, current position (`balance`,
  `unapplied_credit`, `outstanding_total`), ledger `entries` (oldest first with
  `running_balance`), and `outstanding_purchases` (oldest first with `charged`,
  `allocated`, `outstanding`) all come from one consistent snapshot with zero
  write locks.
- **`GET /api/v1/suppliers/{id}/payments`**: write log of supplier payments,
  newest first (`?limit=`, default 50, clamped to `1..=200`), joining wallet,
  payment method, and note from `wallet_movements` and batch-loading
  allocations inside a `REPEATABLE READ, READ ONLY` transaction.
- **`GET /api/v1/suppliers/{id}/adjustments`**: write log of manual
  adjustments, newest first (`?limit=`, default 50, clamped to `1..=200`), with
  signed `amount`, inside a `REPEATABLE READ, READ ONLY` transaction.
- **`GET /api/v1/accounts/payables`**: opens a `REPEATABLE READ, READ ONLY`
  transaction and lists every supplier with `balance != 0` or
  `unapplied_credit != 0` (ordered by `balance DESC, lower(name) ASC, id ASC`),
  plus portfolio totals (`total_balance`, `total_unapplied_credit`,
  `total_outstanding`, `total_adjustments`) and 4 aging buckets (`0-30`,
  `31-60`, `61-90`, `90+`) computed from each open purchase's
  `purchases.completed_at` timestamp in whole elapsed days
  (`FLOOR(EXTRACT(EPOCH FROM (NOW() - completed_at)) / 86400)`). There is no
  due date in this delivery.
- **`GET /api/v1/suppliers` and `GET /api/v1/suppliers/{id}`**: include derived
  `balance` and `available_credit` alongside `credit_limit`.

## API

All routes require a session (`401` without a cookie) and no additional role
gate: paying suppliers, recording credit refunds or adjustments, and reading
statements or payables are operational work, matching catalog, purchases,
payments, and suppliers. Errors use `application/problem+json` (RFC 7807).

| Method | Route | Result |
|---|---|---|
| GET | `/api/v1/suppliers/{id}/account` | `200` `SupplierAccountResponse` (position, oldest-first entries with running balance, open purchases); `404` unknown supplier |
| GET | `/api/v1/suppliers/{id}/payments?limit=` | `200` `SupplierPaymentLogResponse[]` (newest first, `limit` 1..=200); `404` unknown supplier |
| POST | `/api/v1/suppliers/{id}/payments` | `201` fresh / `200` replay `SupplierPaymentResponse`; `404` unknown supplier or purchase; `409` busy account or cross-kind key collision; `422` invalid method, amount, key or allocation |
| POST | `/api/v1/suppliers/{id}/credit-refunds` | `201` fresh / `200` replay `SupplierCreditRefundResponse`; `404` unknown supplier; `409` busy account or cross-kind key collision; `422` invalid method, blank note, bad amount or insufficient credit |
| GET | `/api/v1/suppliers/{id}/adjustments?limit=` | `200` `SupplierAdjustmentLogResponse[]` (newest first, `limit` 1..=200); `404` unknown supplier |
| POST | `/api/v1/suppliers/{id}/adjustments` | `201` `SupplierAdjustmentResponse`; `404` unknown supplier; `409` busy account; `422` blank note, bad amount or balance crossing below zero |
| GET | `/api/v1/accounts/payables` | `200` `PayablesResponse` (portfolio totals, aging buckets, open-position suppliers) |

Purchase completion (`POST /api/v1/purchases/{id}/complete`) and supplier
management (`GET/POST/PATCH /api/v1/suppliers`,
`POST /api/v1/suppliers/{id}/deactivate`) participate in the same ledger and
are summarized in `docs/purchases.md` and `docs/suppliers.md`.

## What is missing

- **Due dates and payment terms**: aging buckets count elapsed days from
  `purchases.completed_at`; there is no per-supplier term or per-purchase due
  date.
- **Interest or late fees**: balances do not accrue interest automatically.
- **Credit refund and adjustment UI forms**: the frontend covers on-account
  purchase completion (`/operations/purchases`), supplier list balances and
  credit limits (`/suppliers`), supplier drawer credit limit / statement /
  pay-supplier form, and the `/payables` aging screen; credit refunds and
  manual adjustments are API-only in this delivery.
- **Statement export**: statements and payables are viewed in the UI or via
  JSON, not exported to PDF or CSV.
