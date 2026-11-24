# Purchase returns (supplier returns)

## The central decision: the return is a document, the purchase is a fact

A completed purchase is never edited, cancelled, or reopened. Returning
merchandise to a supplier is recorded as an append-only **purchase return**
(`purchase_returns` + `purchase_return_items`, migrations `0030`–`0031`), linked
to the completed purchase, with its own lines, reason, stock outflows, and
financial settlement.

Related docs: `docs/purchases.md` (purchase lifecycle and completion),
`docs/supplier-accounts.md` (payables ledger, advance credit, and credit
refunds), `docs/payments.md` (wallets and `purchase_refund` inflows),
`docs/catalog.md` (`purchase_return` stock outflows), and `docs/credit-notes.md`
(customer returns mirror).

## Core invariants

1. **Bound to the linked purchase by design.** Only `completed` purchases can be
   returned (`409` on `draft`/`cancelled`, `404` on unknown purchase). Returned
   lines must reference `product_id`s that exist on `purchase_items` (`422`
   otherwise). Cumulative returned quantity per product across all returns of
   the purchase can never exceed `purchase_items.quantity` (`409` naming the
   remaining returnable quantity).
2. **Frozen unit cost, server-computed total.** The client sends only
   `{product_id, quantity}` — never a cost or total. The server copies each
   line's frozen `unit_cost` from `purchase_items`, computes
   `total = (SUM(qty * unit_cost)).round_dp(2)`, and rejects any return where
   `already_returned + total > purchase.total` (`409` naming the money
   allowance left). Zero-cost promotional lines (`unit_cost = 0`) are valid and
   produce `total = "0.00"` with no wallet or account entries.
3. **Returned goods leave stock with an availability check.** Every returned
   line writes a `stock_movements` row with `reason = 'purchase_return'` (sign
   `-1`), positive `quantity`, and the frozen `unit_cost`. Before any write, the
   transaction locks `products FOR UPDATE` in deterministic `product_id ASC`
   order and verifies `available_stock >= quantity`; insufficient stock rejects
   the whole return with `409`.
4. **Debt is cancelled first; the operator chooses how to settle paid surplus.**
   Under the supplier advisory lock (seed `1`), the return computes:
   - `debt_cancelled = min(return_total, max(purchase_outstanding, 0))` —
     written as one negative `purchase_return` entry on
     `supplier_account_entries` linked to both `purchase_id` and
     `purchase_return_id` (unique per return).
   - `surplus = return_total - debt_cancelled` — handled according to the
     operator's explicit `surplus_mode`:
     - **`wallet_refund`** (default): splits `surplus` proportionally in integer
       cents across the tenders `(wallet_id, payment_method_id)` that paid for
       the purchase (`purchase_payment` movements plus allocated `payment`
       entries) using each tender's **remaining refundable capacity**
       (`paid - prior_purchase_refunded`), writing `purchase_refund` (`+1`
       inflow) wallet movements carrying `purchase_id`, `payment_method_id`, and
       `purchase_return_id`. Allocating against remaining capacity eliminates
       the multi-tender Alabama paradox across sequential returns. If remaining
       wallet capacity is smaller than `surplus` (for instance, because the
       purchase was settled with prior supplier credit rather than wallet
       money), the return is refused whole with `409` naming the remaining
       wallet capacity so the operator can choose `supplier_credit` instead.
     - **`supplier_credit`**: requires an active supplier (`409` if inactive)
       and writes one negative `purchase_return_credit` entry on
       `supplier_account_entries` (`purchase_id = NULL`,
       `purchase_return_id = return.id`), increasing `unapplied_credit (U)` and
       decreasing `balance` by `surplus`. Future on-account purchases
       automatically consume `purchase_return_credit` entries oldest-first via
       `apply_available_credit`, and `POST /suppliers/{id}/credit-refunds` can
       later refund any remaining credit back into a wallet.
5. **One lock order, idempotent retries, atomic rollback.** Every return locks
   in a deadlock-free hierarchy —
   `purchases FOR NO KEY UPDATE` → `products FOR UPDATE (product_id ASC)` →
   supplier advisory lock (seed `1`, 5s timeout → `409`) → optional
   `suppliers FOR UPDATE` — preventing deadlocks with concurrent completions,
   sales, supplier payment allocations (`FOR KEY SHARE` on `purchases`), and
   deactivations. Optional `idempotency_key` (trimmed; blank or `> 100`
   chars → `422`), unique per `(purchase_id, idempotency_key)`, answers `201` on
   first create and `200` on replay without writing duplicate rows. Any failure
   rolls back cleanly with zero side effects.

## API

All routes require an authenticated session (`401` without a cookie) and use
RFC 7807 `application/problem+json` errors. Decimals are strings in both
directions.

| Method | Route | What it does |
|---|---|---|
| GET | `/api/v1/purchases/{id}/returns` | `200` `PurchaseReturnsSummaryResponse`: purchase totals, `purchase_outstanding`, `wallet_refundable_capacity`, returnable lines, and issued returns in a `REPEATABLE READ, READ ONLY` snapshot (`404` unknown purchase) |
| POST | `/api/v1/purchases/{id}/returns` | Creates a return from `{reason, items: [{product_id, quantity}], surplus_mode?, idempotency_key?}`; `201` fresh / `200` replay `PurchaseReturnResponse` |

## UI

- `/operations/purchases` exposes a **Return** action on every completed
  purchase (in both the history table and the detail sheet) leading to
  `/operations/purchases/{id}/return`.
- `/operations/purchases/{id}/return` shows purchase total, already returned,
  open payable, wallet refund capacity, returnable lines, a live preview of debt
  cancelled vs paid surplus, the explicit `wallet_refund` vs `supplier_credit`
  selector, a plain-HTTP-safe idempotency key preserved across failed submits,
  and the table of issued returns.

## What is missing

- **Fiscal debit/credit note numbering**: returns carry an internal UUID rather
  than a numbered fiscal document.
- **VAT breakdown**: return totals match the frozen purchase line costs without
  tax line itemization.
- **Independent supplier debit notes without merchandise**: non-inventory
  supplier rebates or price adjustments use `POST /suppliers/{id}/adjustments`.
