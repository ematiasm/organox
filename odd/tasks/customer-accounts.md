# Customer accounts (credit sales) — design record

Scope: a sale may leave part of its total owed by the customer, the customer's debt is a
first-class ledger, and money is collected against it later. Nothing here is implemented yet.

## Specs

### S1 — The debt ledger is its own append-only book

`customer_account_entries` measures debt; `wallet_movements` keeps measuring real money. A
wallet movement is written only when money actually enters or leaves a wallet.

Columns: `id uuid pk`, `customer_id uuid not null references customers(id) on delete restrict`,
`kind customer_account_entry_kind not null`, `amount numeric(14,2) not null`,
`sale_id uuid null references sales(id) on delete restrict`,
`credit_note_id uuid null references credit_notes(id) on delete restrict`,
`wallet_movement_id uuid null references wallet_movements(id) on delete restrict`,
`idempotency_key text null`, `note text null`,
`created_by uuid not null`, `created_at timestamptz not null default now()`.

`created_by` deliberately has no foreign key to `users`, for the same reason
`wallet_movements.created_by` has none: the ledger is immutable, so deleting an
account must not be able to rewrite or orphan history.

`amount` is signed, and the sign is its effect on the balance:

| kind | sign | meaning | required link | forbidden links |
|---|---|---|---|---|
| `sale_charge` | + | a sale went on account | `sale_id`, unique per sale | note, wallet movement |
| `adjustment_in` | + | manual increase of the debt | `note` non-blank | all |
| `credit_refund` | + | money handed back against a credit balance | `wallet_movement_id` | sale, note |
| `receipt` | − | money collected against the account | `wallet_movement_id`, unique per movement | sale, note |
| `credit_note` | − | a note cancelled part of this debt | `sale_id` + `credit_note_id`, unique per note | wallet movement |
| `adjustment_out` | − | manual decrease of the debt | `note` non-blank | all |

A trigger refuses `UPDATE` and `DELETE` with the same shape as
`wallet_movements_no_update`. Balance is `COALESCE(SUM(amount), 0)::numeric(14,2)`; positive
means the customer owes money. There is no stored balance and no endpoint that writes one.

### S2 — Two derived quantities, one invariant

Let `C` = sum of `sale_charge`, `N` = sum of `|credit_note|`, `Adj` = signed sum of adjustments,
`R` = sum of `|receipt|`, `F` = sum of `credit_refund`, `A` = sum of the allocations in S3.

- `unapplied_credit U = R − A − F`, never negative: money received, not allocated to a sale,
  not handed back.
- `outstanding(sale) = charge(sale) − allocations(sale) − cancel(sale)`, never negative.
- Invariant, asserted by a test after every operation:
  `balance + U == Σ outstanding + Adj`.

Because debt and credit share one signed balance, a customer can be a debtor and hold
unapplied money at the same time; `U` is what makes the two readable separately.

### S3 — Collections are allocated to sales, never invented

`customer_account_allocations(id, entry_id references customer_account_entries on delete restrict,
sale_id references sales on delete restrict, amount numeric(14,2) not null, UNIQUE (entry_id, sale_id))`.
Only a `receipt` entry may own allocations; every allocation targets a completed sale of the same
customer; `amount > 0`.

Rules, all enforced inside one transaction holding the customer lock of S5:

- `Σ allocations(entry) <= |entry.amount|`, enforced by a deferred constraint trigger.
- `Σ allocations to a sale <= outstanding(sale)`.
- `Σ allocations from receipts <= U` (equivalently `A + F <= R` per customer).
- The remainder of a receipt stays unallocated and becomes available credit. It is never a
  second entry and never a second table.

### S4 — A sale can be completed with part of its total on account

`POST /sales/{id}/complete` gains `on_account` (decimal, default `0`, `>= 0`, at most two
decimals). The completion invariant becomes:

`Σ wallet payments + on_account == total`

Rules:

- `on_account > 0` requires the draft to have an active customer, else a typed rejection with no
  state change.
- The completion transaction writes exactly one `sale_charge` of `+on_account` linked to the sale,
  in the same transaction as the stock movements and the wallet movements. A unique partial index
  `(sale_id) WHERE kind = 'sale_charge'` makes a double charge impossible even on retry.
- `on_account == 0` behaves exactly as today; every existing sale-completion test keeps its
  meaning. The empty-payment-list test is renamed to say "unless the account covers it".
- A zero-total sale keeps requiring `on_account = 0`.
- No settlement column is added to `sales`: paid, partially paid and unpaid are derived from
  `outstanding(sale)`, exactly as the balance is derived.

### S5 — Every balance-changing write takes one lock, first

Each transaction that can change a customer's balance takes
`pg_advisory_xact_lock(hashtextextended(customer_id::text, 0))` as its **first** statement,
before any sale or product row lock. One lock order, no deadlock with the existing completion
transaction, and concurrent collections cannot over-collect.

`POST /customers/{id}/receipts` accepts `idempotency_key` (body, optional) with a partial unique
index `(customer_id, idempotency_key) WHERE idempotency_key IS NOT NULL`. A replay returns the
first receipt unchanged with `200`; a fresh write answers `201`. Same contract as credit notes.

### S6 — A collection is money in a wallet and debt down, together

`POST /customers/{id}/receipts` body:
`{ idempotency_key?, payment_method_id, amount, allocations: [{sale_id, amount}], note? }`.

- `amount > 0`, at most two decimals; the payment method must exist and be active.
- Writes one `wallet_movements` row of kind `account_payment` (sign +1) carrying `customer_id`
  and `payment_method_id`, with `sale_id` and `credit_note_id` null, plus one `receipt` entry
  linked to it, in one transaction.
- Every allocation is validated against S3; a rejection leaves no wallet movement and no entry.
- The server never invents allocations. The UI may prefill oldest-first; it is a convenience, not
  a default the API applies.
- Response: the receipt, the resulting balance, `U`, and the outstanding of each allocated sale.

`wallet_movements` gains `customer_id`, constrained so `account_payment` requires it and every
other kind forbids it. `account_payment` must be added to every SQL `CASE` sign mapping
(`src/payments/repository.rs:112-119`, `:151-158`, `src/sales/repository.rs:512-521` is stock).
`every_kind_is_counted_with_its_own_sign` must name the new kind.

### S7 — Credit limit: NULL unlimited, 0 cash only

`customers.credit_limit numeric(14,2) NULL`, `CHECK (credit_limit >= 0)`; `NULL` (the default)
means no limit, `0` means cash only. Exposure after the sale is `balance + on_account`; when the
limit is not null and exposure would exceed it, the completion is rejected whole, with the
remaining amount named in the message, using the same status the stock rejection uses.

Lowering a limit below the current balance is allowed and blocks only new credit. The check runs
under the S5 lock, together with the activity gate: the completion locks the customer advisory
lock first and then reads the customer row `FOR UPDATE`, so a deactivation cannot slip between the
gate and the charge. Deactivation takes only that row lock and never the advisory one, so the two
orders cannot invert. A customer holding credit (`balance` negative) absorbs the new charge before
the limit applies, because using money already in their favour is not credit.

### S8 — Available credit is applied at completion

When a sale puts `on_account` on the account, the same transaction allocates from receipts,
oldest first, `min(on_account, U, on_account − already allocated)` to that sale. It is automatic,
not a flag, and it always shows as one line in the statement. The response exposes
`credit_applied`, and `on_account − credit_applied` is the debt the sale adds.

### S9 — A credit note cancels debt before it pays cash

For a note of `N` on a sale with `outstanding = O`:

- `cancel = min(N, O)` writes one `credit_note` entry of `−cancel`; when `cancel == 0` no entry is
  written and the sale is unchanged.
- `cash = N − cancel` is refunded over the tenders that actually collected money for that sale:
  its original `sale_payment` movements plus the receipts allocated to it, each capped at
  `collected(tender) − already_refunded(tender)`, split in cents with largest-remainder, and the
  shares must sum to `cash` exactly.
- A fully unpaid credit sale therefore produces no wallet movement at all; the note is not refused
  for lacking payments.
- The ceiling `Σ notes <= sale.total` is unchanged, and `already_refunded(tender)` is read from
  `credit_notes` and the refund movements, not recomputed from scratch.
- `allocate_refund` is rewritten around that capacity list and keeps its zero-share skip.

### S10 — Credit is returned with a wallet movement, never by editing the balance

`POST /customers/{id}/credit-refunds` body `{ payment_method_id, amount, note, idempotency_key? }`
writes one `wallet_movements` row of kind `account_payment_refund` (sign −1) and one
`credit_refund` entry of `+amount`. It is refused unless `amount <= U` and `balance + amount <= 0`:
money is only handed back against money actually held in the customer's favour.

### S11 — Adjustments never cross zero

`POST /customers/{id}/adjustments` body `{ direction: in|out, amount, note }` writes
`adjustment_in` or `adjustment_out`. Both require the balance to stay `>= 0`: an adjustment may
increase or decrease a debt, never create or destroy an available credit. This is the only route
that can load an opening balance for a customer who already owed money before the system.

### S12 — A customer with an open position cannot be deactivated

`POST /customers/{id}/deactivate` answers a typed conflict naming the balance while
`balance != 0` or `U != 0`. The operator settles or refunds first.

### S13 — Read surfaces

- `GET /customers/{id}/account`: the entries oldest first with a running balance, the balance, `U`,
  and the outstanding of every sale with debt.
- `GET /customers/{id}/receipts`, `GET /customers/{id}/adjustments`: the write logs of S6 and S11.
- `GET /accounts/receivables`: customers with `balance != 0 or U != 0`, with aging buckets computed
  from the sale date (0-30, 31-60, 61-90, 90+). No due date in this delivery.
- `GET /customers` and `GET /customers/{id}` expose `balance`, `available_credit` and
  `credit_limit`.

### S14 — Surfaces, contracts and generated files

Frontend: `on_account` field in the POS with the resulting amount to charge and the remaining
limit; apply-credit line; customer drawer gains balance, available credit, credit limit, statement
and the receipt form; customers list gains a balance column; a receivables page.

Documentation contracts that currently promise the opposite and must be rewritten, not appended to:
`docs/sales.md:102-105` and `:122-125`, `docs/payments.md:90-95` and `:128-130`,
`docs/credit-notes.md:105-106`, `docs/customers.md:54-56` and `:190-193`. New
`docs/customer-accounts.md`, README feature list, `openapi.json` and
`frontend/src/lib/api/schema.d.ts` regenerated with `make fe-schema`.

### S15 — Tests

New `tests/customer_accounts.rs`. It must pin, through the HTTP surface: the S2 invariant after a
scripted sequence of charge, receipt, partial allocation, credit application, note and adjustment; a
replayed receipt creating no second movement; two concurrent receipts unable to over-collect; two
concurrent completions unable to exceed the limit; over-allocation and allocation to another
customer's sale each leaving no trace; the append-only trigger; the double-charge index; the note
that cancels only debt writing no wallet movement; the note split whose shares sum exactly to the
cash portion; the credit refund that would leave the balance positive being refused; and the
deactivation refusal. Existing suites must pass untouched except for the renamed empty-payment test
and the new `on_account = 0` sibling.

## Tasks

| id | spec | route | commit |
|---|---|---|---|
| T1 | S1, S2 | **done**: `0015` in one migration (type, tables, append-only triggers, sign and link CHECKs, unique charge/note/movement/idempotency indexes, deferred allocation ceiling), `src/customer_accounts/{model,repository}` with the six writers taking the customer advisory lock first, `tests/customer_accounts.rs` (35 tests: table contract, read model, every writer, two concurrent receipts) | `e230572`, `5f5f4f7`, `2cc7768` |
| T2 | S4 | **done**: `complete` takes `on_account`; the invariant is `payments + on_account == total`; the charge is written by `customer_accounts::record_charge` after the status flip inside the same transaction; `on_account = 0` keeps the old path through a four-argument wrapper; 8 HTTP-level tests plus the renamed empty-payment test | `1dc5a6b` |
| T3 | S5, S7 + L12 | **next**: `0017` adds `customers.credit_limit`; the customers module exposes it (NULL = unlimited, `0` = cash only, clearable through PATCH); `customer_accounts::authorize_credit` takes the customer lock, reads the row `FOR UPDATE`, checks activity and exposure (`balance + amount` against the limit, floored at zero for the message) and returns the available amount; sales calls it before writing and answers 409 naming the remaining credit. Closes `R1-on-account-credit-unbounded`, `R3-active-check-race` and `R4-on-account-active-gate-race` | feat/customer-accounts-limit |
| T4 | S3, S5, S6 | **done**: `0018` adds the `account_payment` label alone, `0019` the `wallet_movements.customer_id` column and its two CHECKs; the kind joins both balance `CASE`s and the sign test; `POST /customers/{id}/receipts` answers 201/200 with the replay read under the lock; `ensure_movement_backing` now binds the movement's customer, closing `R1-receipt-backing-not-customer-bound`; 8 HTTP tests plus the ledger one | `4807009` |
| T5 | S8 | credit application at completion | feat/customer-accounts-apply-credit |
| T6 | S9 | debt-first note split, `allocate_refund` rewrite, `account_payment_refund` kind | feat/customer-accounts-note-split |
| T7 | S10, S11, S12 | credit refunds, adjustments, deactivation guard | feat/customer-accounts-adjustments |
| T8 | S13 | statement, receivables with aging, customer read columns | feat/customer-accounts-reads |
| T9 | S14 | POS field, drawer, list column, receivables page, regenerated schema | feat/customer-accounts-ui |
| T10 | S14 | docs, README, openapi regeneration | feat/customer-accounts-docs |
| T11 | S2, S3, S9, S10 + L12 | **next**: close the T1 review advisories — bound a charge to the sale's unpaid part, verify a credit note against its own sale and total, reject a repeated sale in one receipt and a sale-kind movement as backing, map the remaining unique violations, bound the advisory lock wait, fold `outstanding_total` into one statement, add the `allocate` success test, and enforce the per-sale ceiling with a deferred trigger | feat/customer-accounts-advisories |

T1, T2 and T6 are the high-risk slices (irreversible money data, contract change): independent
verification per slice, and each slice stays under roughly 400 authored lines.

## Log

- L1 — "sigamos con los clientes. Lo importante a resolver ahora es el tema de ventas a crédito/cuentas corrientes. Es decir, como llevar los saldos de clientes de ventas que no han sido pagadas. Antes de escribir codigo resolvamos bien todos los problemas que puedan surgir"
- L2 — decisions taken in the design questionnaire: "Imputación por venta (recomendado)", "Permitir saldo a favor", "Si, bloqueo duro pero default = 0 = ilimitado", "Deuda primero, después efectivo (recomendado)".
- L3 — limit sentinel settled: "NULL = ilimitado · 0 = sólo contado". The user's L2 phrase "default = 0" is preserved as intent (no limit by default) with `NULL` as the sentinel, because `0` must remain available for "cash only".
- L4 — design record written before any source change; implementation waits for an explicit go-ahead.
- L5 — scope of T1 settled while implementing it. `wallet_movements.customer_id` and the
  `account_payment` / `account_payment_refund` labels are **not** part of T1: without their writers
  they are dead columns and untestable vocabulary, so they move to T4 and T7, the slices that write
  them. Consequently T1 ships **one** migration instead of two: the `0009`/`0010` split exists only
  because a label added to a *pre-existing* enum cannot be used in the transaction that adds it;
  a brand-new type can be created and used in the same migration. `created_by` carries no foreign
  key to `users`, matching `wallet_movements`.
- L6 — T1 landed in three commits, past the 400-line guidance for a single review slice:
  `e230572` the book and its read model (1472 lines, migration plus tests), `5f5f4f7` charges,
  receipts and allocations (1239 lines), and a third for the note/refund/adjustment writers.
  The size is the table contract: the migration, its triggers and the tests that prove the
  engine rejects a bad row are one indivisible unit. Delivery shape when publishing: three
  chained commits, or three stacked PRs if a reviewer prefers one behavior each. Recorded here
  so the slice boundary is not discovered at merge time.
- L8 — T1 delivered on `feat/customer-accounts-ledger`, not pushed and not reviewed: `e230572`
  (migration, model, reads), `5f5f4f7` (charge, receipt, allocation), `2cc7768` (credit note,
  credit refund, adjustment), plus a docs commit. `make check` green: 287 tests, 35 of them in
  `tests/customer_accounts.rs`, clippy `-D warnings`, svelte-check 0/0. Deliberately absent from
  T1: any HTTP route, any change to sale completion, and the `wallet_movements.customer_id`
  column with its two new kinds — the last two belong to T4 and T7, which write them.
- L9 — the one hole the review of B1 closed is now pinned by a test: a receipt whose amount
  differs from the money its wallet movement carried is refused, because the foreign key proves
  the movement exists, never that the two books agree. The same check guards a credit refund.
- L7 — one hole closed while reviewing B1: `record_receipt` compares the receipt amount with the
  money the wallet movement carried. The foreign key proves the movement exists, never that the
  two books agree, and a silent disagreement between them is the failure nobody notices.
- L10 — native review of T1 at last: **approved**, lineage `review-260435ec1c704443`, target
  `sha256:f411c15d…`, three lenses (risk, resilience, reliability), authority burned with
  `gentle-ai.review-acknowledged/v1`. The candidate is commits `e230572` (book + reads),
  `5f5f4f7` (charge, receipt, allocation), `2cc7768` (note, refund, adjustment) and `f06a2a6`
  (the correction below). An earlier lineage, `review-9bdabf97b891d543`, was abandoned after it
  found a CRITICAL defect and the correction-plan slot refused the provider's own binding in
  both accepted shapes ("missing or stale for current STATUS", nothing mutated); the record sits
  in `.git/gentle-ai/review-transactions/quarantine/review-9bdabf97b891d543-3107567608`.
- L11 — **the critical finding was real and is fixed in `f06a2a6`**: `allocate` bounded an
  allocation only by its own receipt and by the sale's outstanding, never by the customer's
  remaining credit. Receipt 150 (unallocated) → refund 30 → charge 150 → allocate 150 disposed of
  180 against 150 collected, leaving `unapplied_credit` at −30 while `is_consistent()` still
  returned true: the balance identity is algebraically satisfied by an account that spent the
  money twice. Fixed by capping an allocation at `unapplied_credit` and by adding that floor to
  the invariant. The regression test was observed failing before the check was restored.
- L12 — thirteen advisory findings, all non-blocking, none reopening the review. Owner by slice:
  - **T2** `R1-charge-amount-unbounded` — `record_charge` does not bound the charge to the sale
    total; the completion invariant is T2's job and this writer is where it lands.
  - **T4** `R1-movement-backing-amount-only` (the movement is not proven to belong to this
    customer; needs the `wallet_movements.customer_id` column), `R4-receipt-unmapped-unique`,
    `R3-receipt-untyped-rejections`, `R1-duplicate-allocation-untyped-rejection` (a repeated sale
    in one receipt is refused by the engine as an untyped `Database` error).
  - **T5** `R3-allocate-success-untested` — no test drives `allocate` to success; the credit
    application slice is its first real success path, so it must add that test.
  - **T6** `R1-credit-note-document-unverified`, `R3-note-sale-correspondence-unchecked` — the
    writer never checks that the note is about the sale it cancels; the note slice owns the
    document, so it must add the one-SELECT correspondence check.
  - **T8** `R3-position-torn-read`, `R4-account-position-torn-read` — `account_position` reads the
    aggregates and the per-sale list in two snapshots, so a position read outside the lock can
    transiently look inconsistent; fold `outstanding_total` into the same statement.
  - **Engine-level, either T6 or a small follow-up** `R3-per-sale-ceiling-no-engine-guard`,
    `R4-per-sale-ceiling-unenforced` — the per-sale ceiling lives only in Rust, unlike every other
    cross-table rule in `0015`; a deferred constraint trigger would close it.
  - **T4** `R4-advisory-lock-unbounded-wait` — `pg_advisory_xact_lock` waits forever; decide a
    `lock_timeout` when the collection endpoint exposes it.
- L13 — "cerra primero los avisos" (user, verbatim): the advisories are closed before T2.
- L14 — T11 delivered. Closed outright: `R1-charge-amount-unbounded` (a charge now cannot exceed
  `sale.total - its sale_payment movements`), `R1-credit-note-document-unverified` and
  `R3-note-sale-correspondence-unchecked` (the note must be about the sale it cancels and its
  total bounds the cancellation), `R1-duplicate-allocation-untyped-rejection`,
  `R3-receipt-untyped-rejections` and `R4-receipt-unmapped-unique` (typed rejections and the two
  remaining unique violations mapped to `AlreadyRecorded`), `R3-allocate-success-untested`,
  `R3-per-sale-ceiling-no-engine-guard` and `R4-per-sale-ceiling-unenforced` (migration `0016`
  adds the deferred per-sale ceiling trigger, so the last cross-table rule is no longer Rust-only),
  `R3-position-torn-read` and `R4-account-position-torn-read` (`outstanding_total` folded into the
  same statement, pinned by an equivalence test), `R4-advisory-lock-unbounded-wait`
  (`lock_timeout` 5s inside the lock call, `55P03` becomes `CustomerBusy`).
  **Partially closed, on purpose**: `R1-movement-backing-amount-only`. A receipt or refund can no
  longer be backed by a `sale_payment`/`sale_refund` movement, which was the double-collection
  path; proving the movement belongs to *this customer* needs `wallet_movements.customer_id`,
  which arrives with T4. That residual is T4's, not a silent gap.
  Ledger tests went from 36 to 45; `make check` green at 297 tests.
- L15 — T2 delivered in `1dc5a6b`. The completion invariant is now equality: `payments +
  on_account == total`; `on_account > 0` needs a customer on the draft **and** that customer active,
  checked inside the transaction; a customer deactivated mid-draft still does not block a sale the
  till collects in full. The charge goes through `record_charge` **after** the status flip and the
  wallet movements, so its own ceiling (`total − sale_payment`) is exactly the on-account part:
  the ledger re-derives what the completion already proved instead of trusting the caller. Lock
  order stays sale → products → customer advisory; a receipt takes no sale row lock, so there is
  no inversion.
  Two deliberate deviations: `service::complete` keeps its four-argument signature and delegates
  with `Decimal::ZERO`, because `tests/credit_notes.rs` (31 tests) calls it and must keep
  compiling; and the new tests are HTTP-level, which is the only way to pin the JSON field, its
  serde default and the problem details. `sales` still has no settlement column: the ledger is the
  only truth. Generated contract regenerated (`openapi.json`, `schema.d.ts`). Full `make check`
  green at 305 tests, `sales.rs` 42, svelte-check 0/0.
- L16 — T2 reviewed: lineage `review-a7e4951a9f9a438d`, tier **high**, candidate `f06a2a6..HEAD`
  (the advisories commit and T2 together), **approved**, authority burned. Nine advisories, all
  non-blocking. Closed right after the receipt: the negative `on_account` guard and the bounded
  lock wait each got the test they were missing (`a_negative_on_account_is_rejected`,
  `a_busy_customer_account_answers_409`, the latter driving a real `55P03`), and the 409
  description now names the busy account in the contract, the generated document and
  `docs/sales.md`. Still open, each with its owner: **T3** `R1-on-account-credit-unbounded` (no
  aggregate ceiling yet — the credit limit is precisely T3), `R3-active-check-race` and
  `R4-on-account-active-gate-race` (the `is_active` read is not locked, so a deactivation inside
  the window can still be charged; close it with the limit work, which will touch the same gate);
  **T4** `R1-receipt-backing-not-customer-bound`; **accepted, documented** `R4-advisory-wait-holds-checkout-locks`
  and `R4-busy-409-contract-gap` (the 409 is now documented; the checkout serialization is the
  deliberate price of one lock order).
- L17 — second review of the same range, this time with `readability` + `reliability`: lineage
  `review-147b91d960fa0a81` reached `correction_required` over `R2-001` (CRITICAL, deterministic):
  the payment-mismatch detail was rewritten for **every** completion, including a cash-only one
  that never mentions the account, which contradicts "on_account = 0 behaves exactly as before".
  Fixed in `08485a6`: the enriched sentence is emitted only when `on_account > 0`, the variants now
  carry `on_account` so the boundary can decide, and a new HTTP test pins the original wording.
  The finding is real and the previous three-lens pass missed it: no readability lens had run.
  **The correction-plan slot rejected the provider's own binding again**, second lineage, second
  revision, both accepted shapes, `mutation_performed: false` — same class of facade defect as
  `review-9bdabf97b891d543`. Two independent occurrences make it reproducible, not a transcription
  error. The lineage is left for the operator's disposition rather than looped.
- L18 — the trapped lineage `review-147b91d960fa0a81` was abandoned at the operator's decision, after
  the finding was fixed and before any further edit: quarantine
  `review-transactions/quarantine/review-147b91d960fa0a81-2968810132`. Its fix (`08485a6`) enters
  the next review's range together with T3, so nothing it found is lost.
- L19 — T3 reviewed: lineage `review-4c1e09c2daa1a08f`, **approved**, authority burned. Three of its
  five advisories were closed immediately in `ba262f0`; the other two are cosmetic or theoretical
  and stay recorded. Closed: `R1/R3/R4-credit-limit-magnitude-gap` — the trio all flagged that a
  well-formed limit above `numeric(14,2)` reached Postgres and answered an internal error instead of
  a 422, so the boundary now owns the column's magnitude; and
  `R4-credit-limit-display-mislabels-exposure` — `CreditLimitExceeded` said "the charge of" while
  carrying the post-charge exposure; and `R3-authorize-credit-zero-amount` — authorizing nothing is
  always allowed, which matters exactly for the over-limit customers the feature keeps reachable.
  Remaining: nothing actionable beyond the T4 items already listed in L12.
- L20 — **review budget**: the whole branch (`f06a2a6..HEAD`, ~2.9k lines) is over the provider's
  per-lens context budget (`lens_context_budget_exceeded`, no authority created), so reviews must
  name an explicit narrower `baseRef`. The facade also rejects some lens sets with a bare
  `lenses-invalid` before the budget check; the accepted recipe is three lenses
  (`risk`+`resilience`+`reliability`) with a full 40-character commit sha as `baseRef`.
- L21 — the advisory fixes were reviewed on their own, with the small candidate (93 lines) and two
  lenses: lineage `review-8980ab8f447a98f8`, **approved**, authority burned. It found three things
  about the fix itself, all closed in `d28ae60`: the no-op guard used `<= 0` and therefore also
  blessed negative amounts, out of domain for every money writer; and the reworded refusal sentence
  had no assertion of its own, so it could have relapsed without failing a test. The lesson repeats
  in a new shape: a guard that says "nothing to authorize" must say exactly zero, and a changed
  sentence needs a test that reads it.
- L22 — that review of the fixes (`review-a75b458c8f7548eb`, 67 lines, 2 lenses, **approved**) found
  three nits, all in the new tests themselves, and all closed in `8851144`: the sentence test was a
  `#[sqlx::test]` with an unused pool for a pure formatting assertion, and the negative-amount test
  matched the variant without pinning the payload the sentence reads. A test that pays for a
  database it never touches, and a wildcard match over the values an operator reads, are exactly how
  a wording regression survives.
- L23 — T4 delivered in `4807009`: collections are real. Two migrations because the label cannot be
  used in the transaction that adds it (`0018` label alone, `0019` column plus CHECKs), one
  `account_payment` movement carrying the customer, and the ledger receipt written in the same
  transaction with the customer lock taken first and the replay answered under it before any write.
  The residual from L12 is closed: a receipt can no longer be backed by another customer's money,
  while a customer-less manual adjustment still passes so the ledger's fixtures stay honest.
  One frontend line was unavoidable: `KIND_LABELS` is an exhaustive `Record<WalletMovementKind,
  string>`, so a new kind does not compile until it has a label — that is delivering this slice,
  not T9. `make check` green at 333 tests, receipts 8/8, svelte-check 0/0.
- L24 — T4 reviewed: lineage `review-809980264f73e5a2`, tier **high**, **approved**, authority burned.
  Five advisories, two of them money-shaped and both closed in `a11ccde`: a blank idempotency key was
  accepted as a key, so the first collection recorded with one would answer every later blank-key
  collection with that first receipt (cash taken, nothing booked, success reported twice), and the
  100-character bound the contract publishes was documentation only. The amount's positivity and
  precision also moved beside its ceiling — the ledger rejects both, but only after the movement
  exists, and a boundary that publishes a 422 owns the sentence. The replay comment stopped claiming
  a snapshot it never took. Lenses were `risk`+`resilience` only: three lenses no longer fit the
  provider's per-lens budget at 2.4k lines.
- L25 — the blank-key fix was reviewed on its own: lineage `review-465945f2104e6007`, **approved**,
  authority burned, and it caught an over-correction of mine — trimming *every* key, not only the
  blank ones, which rewrote the key namespace: a receipt recorded under a padded key would stop
  matching its own retry and book a second collection, the very double-booking the rule existed to
  prevent. Fixed in `82f02ba`: only keys blank after trimming are dropped; anything else keeps its
  exact bytes and the cap measures those. The amount guards now pin their sentences, so the
  published precedence (sign, precision, magnitude) is asserted rather than implied. Two lenses
  (`risk`+`reliability`) at 172 lines; `risk`+`resilience`+`reliability` answered `lenses-invalid`
  for this candidate, the same rejection that pair of combinations showed earlier.
