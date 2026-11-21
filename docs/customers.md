# Customers

The registry of the people the business sells to, and the one place the
sale ↔ customer link is documented from the registry's side. The sale's side
(`docs/sales.md`) covers what the link does to a checkout.

## Deactivated, never deleted

There is no route that deletes a customer, and there never will be one without
a fresh decision. The only mutation that takes a customer out of circulation is
`POST /customers/{id}/deactivate`, which sets `is_active = false` and answers
`204` — idempotent: an already-inactive customer answers `204` again, not an
error. Reactivation exists, but only as a field: `PATCH {"is_active": true}`;
there is no dedicated activate route.

Why: `sales.customer_id` points at this row (`0014_sales_customer.sql`), and
credit notes hang off sales. History points at a customer, and a row that
disappears breaks it — the same reasoning that keeps a product with movements
alive. The foreign key is `ON DELETE RESTRICT`: the last line of defense if a
`DELETE` ever appears, and a no-op while none does.

The accepted cost: a customer registered by mistake can only be edited or
deactivated. Deactivating does not free the document — the unique index below
matches inactive rows too — but `PATCH` can correct it, so a mistyped document
is fixed, never worked around with a second row.

## The document is optional, unique, and case-blind

`tax_id` is optional: a walk-in buyer is worth registering even without a
document, so the column is nullable. When present, it is unique on
`upper(tax_id)` through a **partial** index (`WHERE tax_id IS NOT NULL`):

- **Partial** because any number of customers may have no document; only
  two with the same one are impossible.
- **`upper(tax_id)`** because a document has no meaningful case: a
  case-sensitive index would let the same taxpayer in twice by spelling the
  document differently.

An empty string never reaches the database: the service trims it and turns it
into `NULL` before writing, because `""` and `NULL` must not be two ways of
saying "no document" — the partial index treats them differently. The same
normalization applies to `email` and `phone`.

The index name is part of the contract: the repository maps a violation of
`customers_tax_id_unique` to the `409` "the document is already in use", so
renaming it breaks the error, not just the schema.

## The fields, and why the registry stops there

`name`, `tax_id`, `email`, `phone` — the original four, chosen explicitly
("Nombre + documento + email + teléfono"), and the reason they stop there is
that nothing else is read. An address, a birth date or a free-text note is a
form the operator fills for the database's sake; each one is justified the day
a feature reads it, not before. The registry has since gained exactly one
more field, `credit_limit`, and for the same reason: the checkout reads it.

A customer still carries no derived state: what it bought, returned or owes
is read from `sales`, `credit_notes` and the account ledger when such a view
exists, never stored twice here. The credit limit is not derived state either
— it is the operator's decision about how much may be owed, a policy rather
than a balance.

The optional fields are deliberately loose where the cost of being wrong is a
blocked sale:

- **`email`** requires exactly one `@` with text on both sides and nothing
  else. A stricter grammar rejects addresses that are valid, and only the
  mailbox owner can say whether an address is reachable — the server never
  claims it is.
- **`phone`** has no format at all, only a length bound. Phone formats differ
  per country and per habit, and a rejected valid number blocks a sale.

## The credit limit: NULL unlimited, 0 cash only

`credit_limit` is the ceiling the shop is willing to be owed by one customer:
`numeric(14, 2)`, optional, never negative, at most two decimals. `NULL` — the
column default, and what every customer created before the field exists has —
means **no limit**: the customer can owe any amount. `0` means **cash only**:
the customer may still buy, but nothing on account. `0` is a limit, not the
absence of one, which is exactly why `NULL` had to be the unlimited sentinel.

It is enforced at checkout, not on the customer row:
`POST /sales/{id}/complete` rejects a completion whose on-account part would
push the customer's balance past the limit — the balance already carried plus
the new amount — with `409` naming the credit that remains (`docs/sales.md`).
Because the check counts the balance, the limit is a ceiling on **total
exposure**, never on the single sale, and a customer who holds money in their
favour can buy on account against it: spending money already in the shop's
hands is not credit.

Lowering a limit below the balance a customer already has is allowed and
blocks only new credit: the existing debt is untouched — it is collected or
cancelled like any other — and the next on-account sale is refused until it
fits.

Unlike the text fields, the limit is **clearable through `PATCH`**: an omitted
`credit_limit` leaves it untouched, an explicit `null` returns the customer to
unlimited. A limit is the one field a real case has asked to clear — an
operator who raised a limit for a good month must be able to undo it without
re-creating the row.

## `include_inactive`, not an `active` filter

The list shows only active customers unless the caller sends
`include_inactive=true`. An `active=true/false` filter was the alternative, and
it loses twice: the default case is "active only", so an `active` filter forces
every caller to state the default explicitly, and an `active=false` listing is
a shape nobody has asked for. `include_inactive` is also the flag the products
list already uses, so the screens learn one word. Deactivation hides a
customer from the default listing, never from `GET /customers/{id}`, which
returns it either way — detail and history keep working.

## `PATCH` replaces; the text fields cannot be cleared

`PATCH /customers/{id}` updates only the fields it receives: the `UPDATE` is
`COALESCE($n, column)`, so an absent text field — which deserializes to SQL
`NULL` — means "leave it as it is". The consequence is that `tax_id`, `email`
and `phone` can be **replaced but not removed**: there is no body that clears
them, and an empty string is normalized to "nothing was sent", so it is
ignored too. A client must therefore send only the fields it wants to change
and never offer a "clear this field" affordance for them; the screen doesn't.
The alternative — reading `null` as "clear" — makes every partial update from
a form erase the fields the form did not send.

`credit_limit` is the one deliberate exception, because it is the one field a
real case has asked to clear: its `UPDATE` carries a separate clear flag, so
an explicit `null` sets the limit back to unlimited while every other field
keeps the `COALESCE` rule above. The distinction lives in serde — an omitted
field and an explicit `null` arrive as different values — and is pinned by
`a_credit_limit_is_optional_and_can_be_cleared`.

## The sale link: optional, and only while it is a draft

`PUT /sales/{id}/customer` attaches or clears the customer, with the body
`{"customer_id": "<uuid>"}` or `{"customer_id": null}`. The field must be
**present**: an omitted `customer_id` is a `422`, because a truncated payload
would otherwise detach the customer silently and the cashier would only see
it on the ticket. Clearing is the explicit `null`.

The link is optional because a walk-in buyer is still a sale: a `NOT NULL`
column would break every anonymous row already charged and push every counter
to invent a fake "consumidor final" customer. And it is **draft-only**: a
completed or cancelled sale is a fact that takes no new customer (`409`). The
draft guard lives in the `WHERE status = 'draft'` clause of a single
`UPDATE`, not in a read-then-write pair: a sale completed between the two
statements could otherwise land a customer on a closed sale with no snapshot.
The customer must exist (`404`) and be active (`409`) — those checks call the
customers module's own `get_customer`, not a copy of the rule in sales SQL, so
the two cannot drift.

Unlike the item mutations, this route answers `204`, not the recalculated
sale: attaching a customer changes nothing the ticket recalculates.

## The name is snapshotted at completion

While the sale is a draft, `SaleResponse.customer_name` is the **live** name:
a `LEFT JOIN` with `COALESCE(sales.customer_name, customers.name)`, so
renaming the customer shows on the open draft. At checkout, the **same
`UPDATE`** that flips `status = 'completed'` and freezes `total` also freezes
the name onto `sales.customer_name`. After that the snapshot wins: renaming the
customer never rewrites what a charged sale says.

The reason is the one that rules the whole sale slice: a completed sale is a
fact, and `sale_items.unit_price` already freezes the price the same way. The
rejected alternative — always joining the live name — is exactly the history
rewrite this feature exists to prevent. The database pins the pair with
`sales_customer_name_needs_its_customer`: a snapshot cannot exist without the
customer it froze.

## A customer deactivated mid-draft does not block the checkout

Attaching requires an active customer; completing does not re-check. If the
customer is deactivated after the attach, the sale still charges and still
takes the snapshot. The reason: the goods already left, and refusing the
checkout would deadlock the counter on a back-office event neither the cashier
nor the buyer caused. The rejected alternative — re-validating at completion —
turns a registry cleanup into a reason a register cannot close a sale.

## API

All routes require a session (`401` without a cookie). There is no additional
role gate: registering and editing customers is operational work, the same
rule the catalog follows. Errors are `application/problem+json` (RFC 7807)
with the `detail` a client can show as-is.

| Method | Route | Result |
|---|---|---|
| POST | `/api/v1/customers` | `201` with `{id}`; `409` document already in use; `422` invalid input |
| GET | `/api/v1/customers?search=&include_inactive=&limit=&offset=` | `200` `{items, limit, offset, total}` |
| GET | `/api/v1/customers/{id}` | `200` `CustomerResponse`; `404` unknown id |
| PATCH | `/api/v1/customers/{id}` | `204`; `404` unknown id; `409` document taken; `422` invalid input |
| POST | `/api/v1/customers/{id}/deactivate` | `204`, idempotent; `404` unknown id |
| PUT | `/api/v1/sales/{id}/customer` | `204`; `404` unknown sale or customer; `409` closed sale or deactivated customer; `422` `customer_id` omitted |

`PUT /sales/{id}/customer` is served by the sales module; it is documented
here because which customer may be attached is the registry's rule.

The list is ordered by name, not recency: a registry is looked up, not
followed, and a newest-first page would jump every time a customer is added.
`limit` defaults to 50 and is clamped to 1–200; a `limit` of 10 million is a
per-query DoS. `total` is `COUNT(*) OVER ()` computed in the same query as the
page, and when the requested page is past the end the empty result carries no
window value, so the service re-counts: a stale link reports the real registry
size instead of claiming it is empty. `search` is trimmed and matches the name
or the document, case-insensitively, in one box — the operator types either
and must not have to say which. `%`, `_` and `\` are escaped before the pattern
reaches SQL, so a document or a name that contains one is searchable as text
rather than as a wildcard: `src/search.rs` holds that escape and both searches in
the tree use it, which is the only way the two cannot drift.

`CustomerResponse` fields: `id`, `name`, `tax_id`, `email`, `phone`,
`is_active`, `credit_limit` (`null` = unlimited, `"0.00"` = cash only, money
as a string), `created_at`, `updated_at`.

## Validation rules

| Field | Rule | Where |
|---|---|---|
| `name` | required, trimmed, 1–200 chars | service + `customers_name_not_blank` |
| `tax_id` | optional, trimmed, ≤ 32 chars, unique without case when present; empty → `NULL` | service + partial index `customers_tax_id_unique` |
| `email` | optional, trimmed, ≤ 254 chars, one `@` with text on both sides | service |
| `phone` | optional, trimmed, ≤ 40 chars, any format | service |
| `email`, `phone`, `tax_id` blank | stored as `NULL`, never `""` | service + `CHECK`s (`*_not_blank`) |
| `credit_limit` | optional, `>= 0`, at most two decimals; `NULL` = unlimited, `0` = cash only; clearable through `PATCH` with an explicit `null` | service + `customers_credit_limit_not_negative` |

Validation exists to hand the operator a sentence they can act on; the rules
that cannot be broken without corrupting data (blank name, blank document,
duplicate document) are enforced by the database as well.

## What is missing

- **Mandatory customer**: the link is optional by decision; there is no mode
  that refuses an anonymous sale.
- **Per-customer history / statement**: no route answers "what did this
  customer buy, return, or owe".
- **`sales_customer_id_idx` serves no query yet**: created in `0014` for the
  customer-side lookup, and nothing reads it until per-customer history
  exists.
- **Merge / deduplicate**: the same person registered twice under two
  spellings cannot be merged; the unique document is the only deduplicator,
  and only when present.
- **Hard delete**: no route, by decision. A customer created by mistake is
  deactivated, and its document stays taken until the row is edited.
- **Malformed bodies answer `text/plain`**: a malformed JSON body gets axum's
  default rejection instead of problem+json, on every route in the tree, not
  just customers' — a boundary older than this feature, recorded in
  `odd/tasks/customers.md`.
