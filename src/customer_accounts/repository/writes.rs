//! Every write path of the customer account ledger: charges, collections and
//! their allocations, credit notes, credit refunds and adjustments, plus
//! the lock and credit-authorization steps every writer passes through.

use rust_decimal::Decimal;
use sqlx::PgConnection;
use std::collections::HashSet;
use uuid::Uuid;

use super::reads::{account_position_in, outstanding_for_sale_in};
use super::{NewCreditRefund, NewReceipt, RecordedReceipt, RepoError};
use crate::customer_accounts::model::{AccountPosition, CustomerAccountEntryKind};
use crate::db::violated_constraint;

/// An amount a money writer accepts: positive, and no finer than cents. The
/// column is `numeric(14, 2)`, so Postgres would silently round a third
/// decimal instead of rejecting it — and a ledger that rounds what the
/// operator typed is a ledger nobody can audit.
fn ensure_cents(amount: Decimal) -> Result<(), RepoError> {
    if amount <= Decimal::ZERO {
        return Err(RepoError::NonPositiveAmount);
    }
    if amount.round_dp(2) != amount {
        return Err(RepoError::TooManyDecimals { amount });
    }
    if amount > crate::money::MAX_AMOUNT {
        return Err(RepoError::AmountTooLarge {
            max: crate::money::MAX_AMOUNT,
        });
    }
    Ok(())
}

/// One writer, one lock, taken first. The lock is deliberately not the
/// customer's row: the sale-completion transaction already locks the sale and
/// its products, so a collection that locked the customer row first could
/// deadlock against it. An xact-scoped advisory lock gives every
/// balance-changing writer the same first step, and concurrent collections
/// serialize here instead of racing on the sums.
///
/// The wait is bounded: a writer that never finished would otherwise park
/// every later collection behind it forever, so the lock times out and the
/// caller answers "busy, retry". `SET LOCAL` keeps the timeout inside this
/// transaction, and the session default is restored right after — the
/// connection goes back to the pool and must not carry a timeout the pool
/// never set.
pub async fn lock_customer(conn: &mut PgConnection, customer_id: Uuid) -> Result<(), RepoError> {
    let found = sqlx::query!(
        r#"SELECT 1 AS found FROM customers WHERE id = $1"#,
        customer_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    if found.is_none() {
        return Err(RepoError::CustomerNotFound);
    }

    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *conn)
        .await?;

    let locked = sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        customer_id.to_string()
    )
    .execute(&mut *conn)
    .await;

    if let Err(err) = locked {
        // 55P03 is `lock_not_available`: the timeout decided, not a failure.
        if matches!(&err, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03")) {
            return Err(RepoError::CustomerBusy);
        }
        return Err(err.into());
    }

    sqlx::query!("SET LOCAL lock_timeout = 0")
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Authorize one charge before it is written: the gate a completion passes
/// through, and the only place the registry's decisions about credit are
/// enforced.
///
/// The order of the two locks is the whole point. The advisory lock is taken
/// first, so this gate serializes with every other balance-changing writer on
/// the account; then the customer's row is read `FOR UPDATE`, so a
/// deactivation racing the checkout cannot slip between the gate and the
/// charge — it waits for the advisory lock first and then the row lock, the
/// exact same order, so the two orders cannot invert and when deactivation
/// finally runs it sees the new balance (or the next completion sees
/// `is_active = false`).
///
/// The rejection is on exposure, not on the single sale: `balance + amount`
/// against `credit_limit`, where `NULL` means no limit at all and `0` means
/// cash only. A customer holding credit (a negative balance) absorbs the new
/// charge before the limit applies, with no special case: adding a negative
/// balance to the amount is exactly "spending money already in their favour",
/// and using that money is not credit. The message names what is left,
/// floored at zero because a customer whose exposure already exceeds the
/// limit has nothing available, not something negative.
///
/// The position is only read when a limit exists: with no limit the activity
/// gate above is the whole rule, and reading sums nobody will compare is
/// cost without a decision.
pub async fn authorize_credit(
    conn: &mut PgConnection,
    customer_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    lock_customer(conn, customer_id).await?;

    let customer = sqlx::query!(
        r#"
        SELECT is_active,
               credit_limit::numeric(14, 2) AS "credit_limit?: Decimal"
        FROM customers
        WHERE id = $1
        FOR UPDATE
        "#,
        customer_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::CustomerNotFound)?;

    if !customer.is_active {
        return Err(RepoError::CustomerInactive);
    }

    let Some(limit) = customer.credit_limit else {
        // No limit: the registry imposed no ceiling, only the activity rule.
        return Ok(());
    };

    // Authorizing exactly nothing is always allowed: the ceiling compares an
    // exposure that a zero charge does not raise. Without this, a customer
    // whose limit was lowered below what they already owe would answer
    // "refused" to a no-op, which says nothing about the charge and everything
    // about the past. A negative amount is out of domain — every money writer
    // rejects it — so it falls through to the ceiling rather than being
    // silently blessed here.
    if amount.is_zero() {
        return Ok(());
    }

    let position = account_position_in(conn, customer_id).await?;

    let exposure = position.balance.checked_add(amount).unwrap_or(Decimal::MAX);
    if exposure > limit {
        return Err(RepoError::CreditLimitExceeded {
            requested: exposure,
            available: limit
                .checked_sub(position.balance)
                .unwrap_or(Decimal::ZERO)
                .max(Decimal::ZERO),
        });
    }

    Ok(())
}

/// The sale an account operation may point at: it exists, it belongs to this
/// customer, and it is completed. One helper, three writers, so the three can
/// never disagree about what a payable sale is.
async fn ensure_chargeable_sale(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
) -> Result<(), RepoError> {
    let sale = sqlx::query!(
        r#"SELECT customer_id, status::text AS "status!" FROM sales WHERE id = $1"#,
        sale_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::SaleNotFound)?;

    if sale.customer_id != Some(customer_id) {
        return Err(RepoError::SaleOfAnotherCustomer);
    }
    if sale.status != "completed" {
        return Err(RepoError::SaleNotChargeable);
    }
    Ok(())
}

/// Verify that the linked `wallet_movements` row exists, carries the exact
/// amount the entry claims, matches the exact expected customer account kind
/// (`account_payment` for receipts, `account_payment_refund` for credit
/// refunds), and belongs to this customer.
async fn ensure_movement_backing(
    conn: &mut PgConnection,
    customer_id: Uuid,
    wallet_movement_id: Uuid,
    amount: Decimal,
    expected_kind: &str,
) -> Result<Uuid, RepoError> {
    let movement = sqlx::query!(
        r#"SELECT amount, customer_id, wallet_id, kind::text AS "kind!" FROM wallet_movements WHERE id = $1"#,
        wallet_movement_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::MovementNotFound)?;

    if movement.amount != amount {
        return Err(RepoError::ReceiptDoesNotMatchMovement {
            movement: movement.amount,
            receipt: amount,
        });
    }

    if movement.kind != expected_kind {
        return Err(RepoError::MovementNotFromAnAccount {
            kind: movement.kind,
        });
    }

    if movement.customer_id != Some(customer_id) {
        return Err(RepoError::MovementOfAnotherCustomer);
    }

    Ok(movement.wallet_id)
}

/// Charge one sale's unpaid part to the account. The unique partial index is
/// the real double-charge guard — the sale-completion path of a later slice
/// writes the same entry — so a retried completion cannot bill the same sale
/// twice even if it never goes through this function.
pub async fn record_charge(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
    amount: Decimal,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;
    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    // A charge may only carry what the sale still leaves unpaid: the wallet's
    // `sale_payment` movements already collected part of the total at the
    // till, and charging that part again would bill the customer for money
    // they already handed over. The completion slice will require exactly
    // this remainder as its `on_account` equality.
    let sale = sqlx::query!(
        r#"
        SELECT s.total::numeric(14, 2) AS "total!: Decimal",
               COALESCE(
                   (SELECT SUM(amount) FROM wallet_movements
                    WHERE sale_id = s.id AND kind = 'sale_payment'),
                   0
               )::numeric(14, 2) AS "collected!: Decimal"
        FROM sales s
        WHERE s.id = $1
        "#,
        sale_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let available = sale.total - sale.collected;
    if amount > available {
        return Err(RepoError::ChargeExceedsSaleTotal {
            requested: amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, created_by)
        VALUES ($1, $2, 'sale_charge', $3, $4, $5)
        "#,
        id,
        customer_id,
        amount,
        sale_id,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("customer_account_entries_one_charge_per_sale") {
            RepoError::AlreadyCharged
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Refuse an allocation that pays more than its sale still owes, on the same
/// [`outstanding_for_sale_in`] definition the debt list uses.
async fn ensure_within_outstanding(
    conn: &mut PgConnection,
    sale_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }
    Ok(())
}

/// A repeated sale in one receipt is the same allocation twice. The
/// engine's `UNIQUE (entry_id, sale_id)` would refuse it, but as an
/// untyped database error and only after the entry had been written;
/// refusing it here keeps the rejection typed and the ledger untouched.
fn ensure_distinct_sales(allocations: &[(Uuid, Decimal)]) -> Result<(), RepoError> {
    let mut seen = HashSet::new();
    for (sale_id, _) in allocations {
        if !seen.insert(*sale_id) {
            return Err(RepoError::DuplicateAllocation { sale_id: *sale_id });
        }
    }
    Ok(())
}

/// The entry this idempotency key already collected, or none. Read under
/// the customer lock, so a retried collection answers the first entry
/// instead of collecting twice.
///
/// The literal keeps the indentation it had inside `record_receipt`:
/// sqlx hashes the raw query text, so re-indenting it would invalidate the
/// offline cache for a query that did not change.
async fn receipt_replay_entry(
    conn: &mut PgConnection,
    customer_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<Uuid>, RepoError> {
    let first = sqlx::query_scalar!(
        r#"
            SELECT id FROM customer_account_entries
            WHERE customer_id = $1 AND idempotency_key = $2 AND kind = 'receipt'
            "#,
        customer_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(first)
}

/// Each allocation is checked against what its sale still owes before
/// anything is written; the sum is checked last so a receipt that pays one
/// sale too much names the sale, not the sum.
async fn ensure_receipt_allocations_fit(
    conn: &mut PgConnection,
    receipt: &NewReceipt<'_>,
) -> Result<(), RepoError> {
    let mut allocated = Decimal::ZERO;
    for (sale_id, amount) in receipt.allocations {
        ensure_cents(*amount)?;
        ensure_chargeable_sale(conn, receipt.customer_id, *sale_id).await?;
        ensure_within_outstanding(conn, *sale_id, *amount).await?;
        allocated = crate::money::checked_add_amount(allocated, *amount).ok_or(
            RepoError::AmountTooLarge {
                max: crate::money::MAX_AMOUNT,
            },
        )?;
    }
    if allocated > receipt.amount {
        return Err(RepoError::ReceiptOverAllocated {
            allocated,
            collected: receipt.amount,
        });
    }
    Ok(())
}

/// Insert the receipt entry: the ledger's copy of the movement that
/// carried the money.
async fn insert_receipt_entry(
    conn: &mut PgConnection,
    entry_id: Uuid,
    receipt: &NewReceipt<'_>,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, wallet_movement_id, idempotency_key, note, created_by)
        VALUES ($1, $2, 'receipt', $3, $4, $5, $6, $7)
        "#,
        entry_id,
        receipt.customer_id,
        -receipt.amount,
        receipt.wallet_movement_id,
        receipt.idempotency_key,
        receipt.note,
        receipt.created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        // Both mean this money is already in the ledger: the movement already
        // backs an entry, or this idempotency key already collected once.
        // The lock makes them rare; the typed mapping makes them readable.
        Some("customer_account_entries_one_entry_per_movement")
        | Some("customer_account_entries_receipt_idempotency") => RepoError::AlreadyRecorded,
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// Insert the receipt's allocations, one row per sale it pays.
async fn insert_receipt_allocations(
    conn: &mut PgConnection,
    entry_id: Uuid,
    allocations: &[(Uuid, Decimal)],
) -> Result<(), RepoError> {
    for (sale_id, amount) in allocations {
        sqlx::query!(
            r#"
            INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
            VALUES ($1, $2, $3, $4)
            "#,
            Uuid::now_v7(),
            entry_id,
            *sale_id,
            *amount,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Record one collection and its allocations.
///
/// Everything is validated before the first insert, so a rejection leaves no
/// entry and no allocation behind — in an append-only ledger a rejected write
/// must not need a compensating entry. The advisory lock is also what makes
/// the replay lookup race-free: two concurrent copies of the same request
/// serialize, and the second one sees the first one's entry.
pub async fn record_receipt(
    conn: &mut PgConnection,
    receipt: NewReceipt<'_>,
) -> Result<RecordedReceipt, RepoError> {
    ensure_cents(receipt.amount)?;
    lock_customer(conn, receipt.customer_id).await?;

    ensure_movement_backing(
        conn,
        receipt.customer_id,
        receipt.wallet_movement_id,
        receipt.amount,
        "account_payment",
    )
    .await?;

    ensure_distinct_sales(receipt.allocations)?;

    if let Some(key) = receipt.idempotency_key
        && let Some(entry_id) = receipt_replay_entry(conn, receipt.customer_id, key).await?
    {
        return Ok(RecordedReceipt {
            entry_id,
            replayed: true,
        });
    }

    ensure_receipt_allocations_fit(conn, &receipt).await?;

    let entry_id = Uuid::now_v7();
    insert_receipt_entry(conn, entry_id, &receipt).await?;
    insert_receipt_allocations(conn, entry_id, receipt.allocations).await?;

    Ok(RecordedReceipt {
        entry_id,
        replayed: false,
    })
}

/// The receipt entry an allocation spends from: the customer who collected it
/// and what it collected. Read before locking: the entry is append-only, so
/// this row cannot change under us; only the allocation sum below needs the
/// lock.
async fn receipt_entry_for_allocation(
    conn: &mut PgConnection,
    entry_id: Uuid,
) -> Result<(Uuid, Decimal), RepoError> {
    let entry = sqlx::query!(
        r#"
        SELECT customer_id, kind AS "kind: CustomerAccountEntryKind", amount
        FROM customer_account_entries
        WHERE id = $1
        "#,
        entry_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::EntryNotFound)?;

    if entry.kind != CustomerAccountEntryKind::Receipt {
        return Err(RepoError::NotAReceipt);
    }

    Ok((entry.customer_id, -entry.amount))
}

/// What this receipt has already been allocated, and the refusal when one
/// more allocation would exceed the money it collected. Returns the
/// allocated sum so the credit ceiling below can account for it.
async fn ensure_allocation_within_receipt(
    conn: &mut PgConnection,
    entry_id: Uuid,
    collected: Decimal,
    amount: Decimal,
) -> Result<Decimal, RepoError> {
    let already = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0)::numeric(14, 2) AS "already!: Decimal"
        FROM customer_account_allocations
        WHERE entry_id = $1
        "#,
        entry_id
    )
    .fetch_one(&mut *conn)
    .await?;

    if already
        .checked_add(amount)
        .is_none_or(|sum| sum > collected)
    {
        return Err(RepoError::AllocationExceedsReceipt {
            collected,
            allocated: already,
        });
    }

    Ok(already)
}

/// Both the account-level `unapplied_credit` ceiling and this receipt's own
/// unrefunded remainder must cover the allocation: money handed back via a
/// `credit_refund` is attributed to specific receipts, so a refunded receipt
/// cannot be reused to back future sales even when newer receipts keep
/// account-level `unapplied_credit` positive.
async fn ensure_allocation_within_credit(
    conn: &mut PgConnection,
    entry_id: Uuid,
    customer_id: Uuid,
    collected: Decimal,
    already: Decimal,
    amount: Decimal,
) -> Result<(), RepoError> {
    let refunded = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0)::numeric(14, 2) AS "refunded!: Decimal"
        FROM customer_account_credit_refund_allocations
        WHERE receipt_entry_id = $1
        "#,
        entry_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let position = account_position_in(conn, customer_id).await?;
    let entry_available = collected
        .checked_sub(already)
        .and_then(|rem| rem.checked_sub(refunded))
        .unwrap_or(Decimal::ZERO)
        .max(Decimal::ZERO);
    let available = position.unapplied_credit.min(entry_available);
    if amount > available {
        return Err(RepoError::AllocationExceedsCredit {
            requested: amount,
            available,
        });
    }

    Ok(())
}

/// Write the allocation row.
async fn insert_allocation(
    conn: &mut PgConnection,
    entry_id: Uuid,
    sale_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
        VALUES ($1, $2, $3, $4)
        "#,
        Uuid::now_v7(),
        entry_id,
        sale_id,
        amount,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        // The same sale cannot be backed twice by one receipt: the constraint is
        // the last line of defence behind the in-memory set `record_receipt`
        // builds, and it must read as a rejected allocation, not a server error.
        Some("customer_account_allocations_once_per_sale") => {
            RepoError::DuplicateAllocation { sale_id }
        }
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// Point already-collected money at one more sale. This is the primitive the
/// credit-application slice will call: it moves no balance, it only says which
/// sale the money pays.
pub async fn allocate(
    conn: &mut PgConnection,
    entry_id: Uuid,
    sale_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    ensure_cents(amount)?;

    let (customer_id, collected) = receipt_entry_for_allocation(conn, entry_id).await?;

    lock_customer(conn, customer_id).await?;

    let already = ensure_allocation_within_receipt(conn, entry_id, collected, amount).await?;

    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    ensure_within_outstanding(conn, sale_id, amount).await?;

    ensure_allocation_within_credit(conn, entry_id, customer_id, collected, already, amount)
        .await?;

    insert_allocation(conn, entry_id, sale_id, amount).await
}

/// One receipt that still carries unallocated and unrefunded money, and how
/// much of it the next allocation or refund may consume.
struct UnappliedReceipt {
    entry_id: Uuid,
    available: Decimal,
}

/// Two ceilings decide before any receipt is read: what the caller asked
/// for, and what the account can actually pay with.
async fn credit_application_cap(
    conn: &mut PgConnection,
    customer_id: Uuid,
    up_to: Decimal,
) -> Result<Decimal, RepoError> {
    let position = account_position_in(conn, customer_id).await?;
    Ok(up_to.min(position.unapplied_credit))
}

/// All receipts that still carry unallocated and unrefunded money, oldest
/// first. The `id` tiebreaker keeps the order total: two receipts created in
/// the same instant are still one before the other, and the ids are
/// time-ordered. Allocations are planned in memory from this locked snapshot
/// and inserted in a single batched query so fragmented accounts neither hit
/// a silent row cap nor pay N+1 query overhead while holding advisory locks.
///
/// Per-receipt refund attributions also exclude refunded receipts so
/// credit-note refunds route through the wallet that actually holds the
/// remaining credit.
async fn unapplied_receipts(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<Vec<UnappliedReceipt>, RepoError> {
    let receipts = sqlx::query!(
        r#"
        SELECT e.id AS "entry_id!: Uuid",
               (
                   -e.amount
                   - COALESCE(alloc.total, 0)
                   - COALESCE(ref_alloc.total, 0)
               )::numeric(14, 2) AS "available!: Decimal"
        FROM customer_account_entries e
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount) AS total
            FROM customer_account_allocations a
            WHERE a.entry_id = e.id
        ) alloc ON true
        LEFT JOIN LATERAL (
            SELECT SUM(ra.amount) AS total
            FROM customer_account_credit_refund_allocations ra
            WHERE ra.receipt_entry_id = e.id
        ) ref_alloc ON true
        WHERE e.customer_id = $1
          AND e.kind = 'receipt'
          AND -e.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
        ORDER BY e.created_at, e.id
        "#,
        customer_id
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(receipts
        .into_iter()
        .map(|receipt| UnappliedReceipt {
            entry_id: receipt.entry_id,
            available: receipt.available,
        })
        .collect())
}

/// The allocations this application plans to write: one row per receipt it
/// spends, and the total it applied.
struct PlannedCreditApplication {
    ids: Vec<Uuid>,
    entry_ids: Vec<Uuid>,
    amounts: Vec<Decimal>,
    applied: Decimal,
}

/// Spend `remaining` across the receipts oldest first, planning one
/// allocation per receipt until the sale is covered.
fn plan_credit_application(
    receipts: Vec<UnappliedReceipt>,
    mut remaining: Decimal,
) -> Result<PlannedCreditApplication, RepoError> {
    let mut ids = Vec::new();
    let mut entry_ids = Vec::new();
    let mut amounts = Vec::new();
    let mut applied = Decimal::ZERO;

    for receipt in receipts {
        if remaining.is_zero() {
            break;
        }
        let take = remaining.min(receipt.available);
        ensure_cents(take)?;
        ids.push(Uuid::now_v7());
        entry_ids.push(receipt.entry_id);
        amounts.push(take);
        applied += take;
        remaining -= take;
    }

    // If the caps derived from the locked snapshot could not be covered by the
    // account's unapplied receipts, the ledger's own invariants disagreed.
    if !remaining.is_zero() {
        return Err(RepoError::CreditApplicationBroken);
    }

    Ok(PlannedCreditApplication {
        ids,
        entry_ids,
        amounts,
        applied,
    })
}

/// Insert the planned allocations in one batched query.
async fn insert_planned_allocations(
    conn: &mut PgConnection,
    sale_id: Uuid,
    planned: &PlannedCreditApplication,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
        SELECT id, entry_id, $3, amount
        FROM UNNEST($1::uuid[], $2::uuid[], $4::numeric(14, 2)[])
            AS planned(id, entry_id, amount)
        "#,
        &planned.ids,
        &planned.entry_ids,
        sale_id,
        &planned.amounts,
    )
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Spend the customer's available credit on one sale, oldest receipt first,
/// and return what was actually applied — `0` when nothing could be.
///
/// Oldest first because a customer's credit is not earmarked: money collected
/// without an allocation belongs to the account as a whole, so picking any
/// receipt but the oldest would be a decision nobody made. The UI prefills its
/// allocation suggestions oldest-first for the same reason, so what the sale
/// applies automatically and what the operator's screen suggests agree on the
/// rule.
///
/// Automatic because a customer holding credit must not be asked for cash:
/// the shop already has their money, and leaving it idle beside a new charge
/// would bill them twice for the same goods. The completion calls this inside
/// its own transaction, so the charge and the payment that covers it commit
/// together.
///
/// The caller must already hold the customer's advisory lock (a completion
/// holds it from its credit gate): every receipt this reads and every
/// allocation it writes depends on sums only that lock keeps stable, so this
/// function never takes the lock itself — a writer that claimed to serialize
/// what its caller already serialized would be promising a guarantee it
/// cannot deliver twice.
pub async fn apply_available_credit(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
    up_to: Decimal,
) -> Result<Decimal, RepoError> {
    let cap = credit_application_cap(conn, customer_id, up_to).await?;
    if cap <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    // The sale is the third ceiling: applying more than it owes would turn
    // credit the customer keeps into a phantom overpayment.
    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    let remaining = cap.min(outstanding);
    if remaining <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    let receipts = unapplied_receipts(conn, customer_id).await?;
    let planned = plan_credit_application(receipts, remaining)?;

    insert_planned_allocations(conn, sale_id, &planned).await?;

    Ok(planned.applied)
}

/// Cancel part of a sale's debt with a credit note. The note is the document,
/// the entry is the ledger's copy of it: one note writes one entry, once, and
/// the unique index on `credit_note_id` is what keeps that true.
///
/// The note's own ceiling — that notes never exceed the sale's total, and how
/// the remainder is refunded in cash — belongs to the credit-note slice that
/// writes the note document. This writer refuses only what the ledger itself
/// forbids: cancelling debt that does not exist.
pub async fn record_credit_note(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
    credit_note_id: Uuid,
    amount: Decimal,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;
    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    // The note is the document this entry copies, so the document decides:
    // it must be about this sale, and it cannot cancel more than it carries.
    // Checked before the ledger's own ceiling, because a note that never
    // belonged to the sale is a mismatch, not unaffordable debt.
    let note = sqlx::query!(
        r#"
        SELECT sale_id, total::numeric(14, 2) AS "total!: Decimal"
        FROM credit_notes
        WHERE id = $1
        "#,
        credit_note_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::CreditNoteNotFound)?;

    if note.sale_id != sale_id {
        return Err(RepoError::CreditNoteMismatch);
    }
    if amount > note.total {
        return Err(RepoError::CreditNoteExceeded {
            requested: amount,
            available: note.total,
        });
    }

    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
        VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)
        "#,
        id,
        customer_id,
        -amount,
        sale_id,
        credit_note_id,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("customer_account_entries_one_entry_per_note") {
            RepoError::AlreadyRecorded
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Hand collected money back to a customer who holds credit. The wallet
/// movement is the money actually leaving; the entry is the ledger's copy,
/// stored positive because a refund moves the balance toward zero.
pub async fn record_credit_refund(
    conn: &mut PgConnection,
    customer_id: Uuid,
    wallet_movement_id: Uuid,
    amount: Decimal,
    note: Option<&str>,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    record_credit_refund_with_key(
        conn,
        NewCreditRefund {
            customer_id,
            wallet_movement_id,
            amount,
            idempotency_key: None,
            note,
            created_by,
        },
    )
    .await
}

/// Money is only handed back against money the shop actually holds: the
/// unapplied credit says it was collected, the negative balance says it was
/// never spent on debt. Either alone could lie — unapplied credit next to
/// a debt, or a negative balance with the credit already refunded — so the
/// refund sees the smaller of the two, floored at zero because a customer
/// who owes has nothing to claim back at all.
fn refundable_credit(position: &AccountPosition) -> Decimal {
    position
        .unapplied_credit
        .min(-position.balance)
        .max(Decimal::ZERO)
}

/// A replay answers by identity, not by re-running the numbers: the first
/// refund already consumed the credit this one claims, so affordability
/// alone could not tell a replay from a fresh refund that happens to be
/// unaffordable. The lock makes the lookup race-free; the unique index
/// stays as the backstop.
async fn ensure_refund_not_recorded(
    conn: &mut PgConnection,
    wallet_movement_id: Uuid,
) -> Result<(), RepoError> {
    let already = sqlx::query_scalar!(
        r#"
        SELECT id FROM customer_account_entries
        WHERE wallet_movement_id = $1
        "#,
        wallet_movement_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if already.is_some() {
        return Err(RepoError::AlreadyRecorded);
    }

    Ok(())
}

/// Insert the ledger's copy of the refund.
async fn insert_credit_refund_entry(
    conn: &mut PgConnection,
    id: Uuid,
    refund: &NewCreditRefund<'_>,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, wallet_movement_id, idempotency_key, note, created_by)
        VALUES ($1, $2, 'credit_refund', $3, $4, $5, $6, $7)
        "#,
        id,
        refund.customer_id,
        refund.amount,
        refund.wallet_movement_id,
        refund.idempotency_key,
        refund.note,
        refund.created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("customer_account_entries_one_entry_per_movement")
        | Some("customer_account_entries_receipt_idempotency") => RepoError::AlreadyRecorded,
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// All receipts that still carry unallocated and unrefunded money, the ones
/// the refunding wallet collected first, then oldest-first across the rest:
/// consuming receipts from the same wallet first preserves the physical
/// wallet correspondence between the money handed back and the money it
/// came from.
async fn unapplied_receipts_wallet_first(
    conn: &mut PgConnection,
    customer_id: Uuid,
    wallet_id: Uuid,
) -> Result<Vec<UnappliedReceipt>, RepoError> {
    let receipts = sqlx::query!(
        r#"
        SELECT e.id AS "entry_id!: Uuid",
               (
                   -e.amount
                   - COALESCE(alloc.total, 0)
                   - COALESCE(ref_alloc.total, 0)
               )::numeric(14, 2) AS "available!: Decimal"
        FROM customer_account_entries e
        LEFT JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount) AS total
            FROM customer_account_allocations a
            WHERE a.entry_id = e.id
        ) alloc ON true
        LEFT JOIN LATERAL (
            SELECT SUM(ra.amount) AS total
            FROM customer_account_credit_refund_allocations ra
            WHERE ra.receipt_entry_id = e.id
        ) ref_alloc ON true
        WHERE e.customer_id = $1
          AND e.kind = 'receipt'
          AND -e.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
        ORDER BY CASE WHEN wm.wallet_id = $2 THEN 0 ELSE 1 END, e.created_at, e.id
        "#,
        customer_id,
        wallet_id,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(receipts
        .into_iter()
        .map(|receipt| UnappliedReceipt {
            entry_id: receipt.entry_id,
            available: receipt.available,
        })
        .collect())
}

/// Attribute the refund across the receipts that hold it, one row per
/// receipt consumed, until the whole refund is placed. A remainder nobody
/// could cover means the caps derived from the locked snapshot disagreed
/// with the ledger's own invariants.
async fn attribute_refund_to_receipts(
    conn: &mut PgConnection,
    refund_entry_id: Uuid,
    receipts: Vec<UnappliedReceipt>,
    amount: Decimal,
) -> Result<(), RepoError> {
    let mut remaining = amount;
    for receipt in receipts {
        if remaining.is_zero() {
            break;
        }
        let take = remaining.min(receipt.available);
        sqlx::query!(
            r#"
            INSERT INTO customer_account_credit_refund_allocations
                (id, refund_entry_id, receipt_entry_id, amount)
            VALUES ($1, $2, $3, $4)
            "#,
            Uuid::now_v7(),
            refund_entry_id,
            receipt.entry_id,
            take,
        )
        .execute(&mut *conn)
        .await?;
        remaining -= take;
    }

    if !remaining.is_zero() {
        return Err(RepoError::CreditApplicationBroken);
    }

    Ok(())
}

/// [`record_credit_refund`] with an optional idempotency key.
pub async fn record_credit_refund_with_key(
    conn: &mut PgConnection,
    refund: NewCreditRefund<'_>,
) -> Result<Uuid, RepoError> {
    ensure_cents(refund.amount)?;
    lock_customer(conn, refund.customer_id).await?;

    // The same checks the receipt makes, for the same reasons: the movement
    // carried what the entry claims, its kind is `account_payment_refund`, and
    // it belongs to this customer.
    let refund_wallet_id = ensure_movement_backing(
        conn,
        refund.customer_id,
        refund.wallet_movement_id,
        refund.amount,
        "account_payment_refund",
    )
    .await?;

    ensure_refund_not_recorded(conn, refund.wallet_movement_id).await?;

    let position = account_position_in(conn, refund.customer_id).await?;
    let available = refundable_credit(&position);
    if refund.amount > available {
        return Err(RepoError::CreditRefundExceedsCredit {
            requested: refund.amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    insert_credit_refund_entry(conn, id, &refund).await?;

    let receipts =
        unapplied_receipts_wallet_first(conn, refund.customer_id, refund_wallet_id).await?;
    attribute_refund_to_receipts(conn, id, receipts, refund.amount).await?;

    Ok(id)
}

/// Record a manual adjustment: the only way to load an opening balance or
/// correct the books by hand. It moves the balance without touching any sale,
/// so it must never create or destroy an available credit — that credit is
/// backed by receipts, and only receipts and refunds may move it.
pub async fn record_adjustment(
    conn: &mut PgConnection,
    customer_id: Uuid,
    kind: CustomerAccountEntryKind,
    amount: Decimal,
    note: &str,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    // The kind is checked before anything is read: an operator cannot smuggle
    // a charge or a receipt through the adjustment form.
    if !kind.is_manual() {
        return Err(RepoError::NotAnAdjustment);
    }
    if note.trim().is_empty() {
        return Err(RepoError::AdjustmentNoteRequired);
    }

    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;

    let position = account_position_in(conn, customer_id).await?;

    // When `balance >= 0`, `AdjustmentIn` adds manual debt freely and
    // `AdjustmentOut` cannot cross zero (`amount <= balance`).
    // When `balance < 0`, `AdjustmentOut` is always refused, while
    // `AdjustmentIn` is permitted up to the non-credit negative drift
    // `(-balance - unapplied_credit).max(0)` so operators can reconcile an
    // over-adjusted account back to zero without burying refundable credit.
    let rejected = if position.balance < Decimal::ZERO {
        match kind {
            CustomerAccountEntryKind::AdjustmentIn => {
                let non_credit_drift = (-position.balance)
                    .checked_sub(position.unapplied_credit)
                    .unwrap_or(Decimal::ZERO)
                    .max(Decimal::ZERO);
                amount > non_credit_drift
            }
            _ => true,
        }
    } else {
        kind == CustomerAccountEntryKind::AdjustmentOut && amount > position.balance
    };

    if rejected {
        return Err(RepoError::AdjustmentCrossesZero {
            balance: position.balance,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries (id, customer_id, kind, amount, note, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        id,
        customer_id,
        kind as CustomerAccountEntryKind,
        amount * Decimal::from(kind.sign()),
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await?;

    Ok(id)
}
