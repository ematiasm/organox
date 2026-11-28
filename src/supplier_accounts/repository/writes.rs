//! Every write path of the supplier account ledger: charges, payments and
//! their allocations, purchase returns and their credits, credit refunds
//! and adjustments, plus the lock and credit-authorization steps every
//! writer passes through.

use rust_decimal::Decimal;
use sqlx::PgConnection;
use std::collections::HashSet;
use uuid::Uuid;

use super::reads::{account_position_in, outstanding_for_purchase_in};
use super::{NewSupplierCreditRefund, NewSupplierPayment, RecordedSupplierPayment, RepoError};
use crate::db::violated_constraint;
use crate::supplier_accounts::model::{SupplierAccountEntryKind, SupplierAccountPosition};

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

/// Serialize every balance-changing writer for a supplier using a bounded
/// transaction-scoped advisory lock (`seed = 1`, disjoint from
/// `customer_accounts`'s `seed = 0`).
pub async fn lock_supplier(conn: &mut PgConnection, supplier_id: Uuid) -> Result<(), RepoError> {
    let found = sqlx::query!(
        r#"SELECT 1 AS found FROM suppliers WHERE id = $1"#,
        supplier_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    if found.is_none() {
        return Err(RepoError::SupplierNotFound);
    }

    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *conn)
        .await?;

    let locked = sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 1))",
        supplier_id.to_string()
    )
    .execute(&mut *conn)
    .await;

    if let Err(err) = locked {
        if matches!(&err, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03")) {
            return Err(RepoError::SupplierBusy);
        }
        return Err(err.into());
    }

    sqlx::query!("SET LOCAL lock_timeout = 0")
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Authorize one on-account purchase charge under the supplier lock order:
/// advisory lock first, then `suppliers FOR UPDATE`.
pub async fn authorize_credit(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    lock_supplier(conn, supplier_id).await?;

    let supplier = sqlx::query!(
        r#"
        SELECT is_active,
               credit_limit::numeric(14, 2) AS "credit_limit?: Decimal"
        FROM suppliers
        WHERE id = $1
        FOR UPDATE
        "#,
        supplier_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::SupplierNotFound)?;

    if !supplier.is_active {
        return Err(RepoError::SupplierInactive);
    }

    let Some(limit) = supplier.credit_limit else {
        return Ok(());
    };

    if amount.is_zero() {
        return Ok(());
    }

    let position = account_position_in(conn, supplier_id).await?;

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

async fn ensure_chargeable_purchase(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_id: Uuid,
) -> Result<(), RepoError> {
    let purchase = sqlx::query!(
        r#"SELECT supplier_id, status::text AS "status!" FROM purchases WHERE id = $1"#,
        purchase_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::PurchaseNotFound)?;

    if purchase.supplier_id != Some(supplier_id) {
        return Err(RepoError::PurchaseOfAnotherSupplier);
    }
    if purchase.status != "completed" {
        return Err(RepoError::PurchaseNotChargeable);
    }
    Ok(())
}

/// Verify that the linked `wallet_movements` row exists, matches the exact
/// expected supplier account kind (`supplier_account_payment` or
/// `supplier_account_refund`), carries the exact amount, and belongs to this
/// supplier.
async fn ensure_movement_backing(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    wallet_movement_id: Uuid,
    amount: Decimal,
    expected_kind: &str,
) -> Result<Uuid, RepoError> {
    let movement = sqlx::query!(
        r#"SELECT amount, supplier_id, wallet_id, kind::text AS "kind!" FROM wallet_movements WHERE id = $1"#,
        wallet_movement_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::MovementNotFound)?;

    if movement.amount != amount {
        return Err(RepoError::EntryDoesNotMatchMovement {
            movement: movement.amount,
            entry: amount,
        });
    }

    if movement.kind != expected_kind {
        return Err(RepoError::MovementNotFromSupplierAccount {
            kind: movement.kind,
        });
    }

    if movement.supplier_id != Some(supplier_id) {
        return Err(RepoError::MovementOfAnotherSupplier);
    }

    Ok(movement.wallet_id)
}

/// Charge one completed purchase's unpaid part to the supplier account.
pub async fn record_charge(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_id: Uuid,
    amount: Decimal,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_supplier(conn, supplier_id).await?;
    ensure_chargeable_purchase(conn, supplier_id, purchase_id).await?;

    let purchase = sqlx::query!(
        r#"
        SELECT p.total::numeric(14, 2) AS "total!: Decimal",
               COALESCE(
                   (SELECT SUM(amount) FROM wallet_movements
                    WHERE purchase_id = p.id AND kind = 'purchase_payment'),
                   0
               )::numeric(14, 2) AS "paid_upfront!: Decimal"
        FROM purchases p
        WHERE p.id = $1
        "#,
        purchase_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let available = purchase.total - purchase.paid_upfront;
    if amount > available {
        return Err(RepoError::ChargeExceedsPurchaseTotal {
            requested: amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, purchase_id, created_by)
        VALUES ($1, $2, 'purchase_charge', $3, $4, $5)
        "#,
        id,
        supplier_id,
        amount,
        purchase_id,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("supplier_account_entries_one_charge_per_purchase") {
            RepoError::AlreadyCharged
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Refuse an allocation that pays more than its purchase still owes, on the
/// same [`outstanding_for_purchase_in`] definition the payable list uses.
async fn ensure_within_outstanding(
    conn: &mut PgConnection,
    purchase_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    let outstanding = outstanding_for_purchase_in(conn, purchase_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }
    Ok(())
}

/// A repeated purchase in one payment is the same allocation twice. The
/// engine's `UNIQUE (entry_id, purchase_id)` would refuse it, but as an
/// untyped database error and only after the entry had been written;
/// refusing it here keeps the rejection typed and the ledger untouched.
fn ensure_distinct_purchases(allocations: &[(Uuid, Decimal)]) -> Result<(), RepoError> {
    let mut seen = HashSet::new();
    for (purchase_id, _) in allocations {
        if !seen.insert(*purchase_id) {
            return Err(RepoError::DuplicateAllocation {
                purchase_id: *purchase_id,
            });
        }
    }
    Ok(())
}

/// The entry this idempotency key already paid, or none. Read under the
/// supplier lock, so a retried payment answers the first entry instead of
/// paying twice.
///
/// The literal keeps the indentation it had inside `record_payment`:
/// sqlx hashes the raw query text, so re-indenting it would invalidate the
/// offline cache for a query that did not change.
async fn payment_replay_entry(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<Uuid>, RepoError> {
    let first = sqlx::query_scalar!(
        r#"
            SELECT id FROM supplier_account_entries
            WHERE supplier_id = $1 AND idempotency_key = $2 AND kind = 'payment'
            "#,
        supplier_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(first)
}

/// Each allocation is checked against what its purchase still owes before
/// anything is written; the sum is checked last so a payment that pays one
/// purchase too much names the purchase, not the sum.
async fn ensure_payment_allocations_fit(
    conn: &mut PgConnection,
    payment: &NewSupplierPayment<'_>,
) -> Result<(), RepoError> {
    let mut allocated = Decimal::ZERO;
    for (purchase_id, amount) in payment.allocations {
        ensure_cents(*amount)?;
        ensure_chargeable_purchase(conn, payment.supplier_id, *purchase_id).await?;
        ensure_within_outstanding(conn, *purchase_id, *amount).await?;
        allocated = crate::money::checked_add_amount(allocated, *amount).ok_or(
            RepoError::AmountTooLarge {
                max: crate::money::MAX_AMOUNT,
            },
        )?;
    }
    if allocated > payment.amount {
        return Err(RepoError::PaymentOverAllocated {
            allocated,
            paid: payment.amount,
        });
    }
    Ok(())
}

/// Insert the payment entry: the ledger's copy of the movement that
/// carried the money.
async fn insert_payment_entry(
    conn: &mut PgConnection,
    entry_id: Uuid,
    payment: &NewSupplierPayment<'_>,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries
            (id, supplier_id, kind, amount, wallet_movement_id, idempotency_key, note, created_by)
        VALUES ($1, $2, 'payment', $3, $4, $5, $6, $7)
        "#,
        entry_id,
        payment.supplier_id,
        -payment.amount,
        payment.wallet_movement_id,
        payment.idempotency_key,
        payment.note,
        payment.created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("supplier_account_entries_one_entry_per_movement")
        | Some("supplier_account_entries_idempotency") => RepoError::AlreadyRecorded,
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// Insert the payment's allocations, one row per purchase it pays.
async fn insert_payment_allocations(
    conn: &mut PgConnection,
    entry_id: Uuid,
    allocations: &[(Uuid, Decimal)],
) -> Result<(), RepoError> {
    for (purchase_id, amount) in allocations {
        sqlx::query!(
            r#"
            INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount)
            VALUES ($1, $2, $3, $4)
            "#,
            Uuid::now_v7(),
            entry_id,
            *purchase_id,
            *amount,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Record one supplier payment and its explicit allocations.
pub async fn record_payment(
    conn: &mut PgConnection,
    payment: NewSupplierPayment<'_>,
) -> Result<RecordedSupplierPayment, RepoError> {
    ensure_cents(payment.amount)?;
    lock_supplier(conn, payment.supplier_id).await?;

    ensure_movement_backing(
        conn,
        payment.supplier_id,
        payment.wallet_movement_id,
        payment.amount,
        "supplier_account_payment",
    )
    .await?;

    ensure_distinct_purchases(payment.allocations)?;

    if let Some(key) = payment.idempotency_key
        && let Some(entry_id) = payment_replay_entry(conn, payment.supplier_id, key).await?
    {
        return Ok(RecordedSupplierPayment {
            entry_id,
            replayed: true,
        });
    }

    ensure_payment_allocations_fit(conn, &payment).await?;

    let entry_id = Uuid::now_v7();
    insert_payment_entry(conn, entry_id, &payment).await?;
    insert_payment_allocations(conn, entry_id, payment.allocations).await?;

    Ok(RecordedSupplierPayment {
        entry_id,
        replayed: false,
    })
}

/// The entry an allocation spends from: a payment or a return credit, the
/// supplier it belongs to, and what it paid. Read before locking: the
/// entry is append-only, so this row cannot change under us; only the
/// allocation sum below needs the lock.
async fn payment_entry_for_allocation(
    conn: &mut PgConnection,
    entry_id: Uuid,
) -> Result<(Uuid, Decimal), RepoError> {
    let entry = sqlx::query!(
        r#"
        SELECT supplier_id, kind AS "kind: SupplierAccountEntryKind", amount
        FROM supplier_account_entries
        WHERE id = $1
        "#,
        entry_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::EntryNotFound)?;

    if !matches!(
        entry.kind,
        SupplierAccountEntryKind::Payment | SupplierAccountEntryKind::PurchaseReturnCredit
    ) {
        return Err(RepoError::NotAPayment);
    }

    Ok((entry.supplier_id, -entry.amount))
}

/// What this payment has already been allocated, and the refusal when one
/// more allocation would exceed the money it paid. Returns the allocated
/// sum so the credit ceiling below can account for it.
async fn ensure_allocation_within_payment(
    conn: &mut PgConnection,
    entry_id: Uuid,
    paid: Decimal,
    amount: Decimal,
) -> Result<Decimal, RepoError> {
    let already = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0)::numeric(14, 2) AS "already!: Decimal"
        FROM supplier_account_allocations
        WHERE entry_id = $1
        "#,
        entry_id
    )
    .fetch_one(&mut *conn)
    .await?;

    if already.checked_add(amount).is_none_or(|sum| sum > paid) {
        return Err(RepoError::AllocationExceedsPayment {
            paid,
            allocated: already,
        });
    }

    Ok(already)
}

/// Both the account-level `unapplied_credit` ceiling and this payment's
/// own unrefunded remainder must cover the allocation: money handed back
/// via a `credit_refund` is attributed to specific sources, so a refunded
/// payment cannot be reused to back future purchases even when newer
/// payments keep account-level `unapplied_credit` positive.
async fn ensure_allocation_within_credit(
    conn: &mut PgConnection,
    entry_id: Uuid,
    supplier_id: Uuid,
    paid: Decimal,
    already: Decimal,
    amount: Decimal,
) -> Result<(), RepoError> {
    let refunded = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0)::numeric(14, 2) AS "refunded!: Decimal"
        FROM supplier_account_credit_refund_allocations
        WHERE source_entry_id = $1
        "#,
        entry_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let position = account_position_in(conn, supplier_id).await?;
    let entry_available = paid
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
    purchase_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount)
        VALUES ($1, $2, $3, $4)
        "#,
        Uuid::now_v7(),
        entry_id,
        purchase_id,
        amount,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        // The same purchase cannot be backed twice by one payment: the
        // constraint is the last line of defence behind the in-memory set in
        // `record_payment`, and it must read as a rejected allocation instead
        // of a server error.
        Some("supplier_account_allocations_once_per_purchase") => {
            RepoError::DuplicateAllocation { purchase_id }
        }
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// Allocate already-paid supplier money to one completed purchase.
pub async fn allocate(
    conn: &mut PgConnection,
    entry_id: Uuid,
    purchase_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    ensure_cents(amount)?;

    let (supplier_id, paid) = payment_entry_for_allocation(conn, entry_id).await?;

    lock_supplier(conn, supplier_id).await?;

    let already = ensure_allocation_within_payment(conn, entry_id, paid, amount).await?;

    ensure_chargeable_purchase(conn, supplier_id, purchase_id).await?;

    ensure_within_outstanding(conn, purchase_id, amount).await?;

    ensure_allocation_within_credit(conn, entry_id, supplier_id, paid, already, amount).await?;

    insert_allocation(conn, entry_id, purchase_id, amount).await
}

/// Spend the business's available unapplied advance credit with this supplier
/// on one newly completed purchase, oldest payment first, returning the amount
/// actually applied (`0` when none was available).
///
/// The caller must already hold the supplier advisory lock (e.g. via
/// [`authorize_credit`]).
pub async fn apply_available_credit(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_id: Uuid,
    up_to: Decimal,
) -> Result<Decimal, RepoError> {
    let position = account_position_in(conn, supplier_id).await?;
    let cap = up_to.min(position.unapplied_credit);
    if cap <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    let outstanding = outstanding_for_purchase_in(conn, purchase_id).await?;
    let mut remaining = cap.min(outstanding);
    if remaining <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    ensure_chargeable_purchase(conn, supplier_id, purchase_id).await?;

    let payments = sqlx::query!(
        r#"
        SELECT e.id AS "entry_id!: Uuid",
               (
                   -e.amount
                   - COALESCE(alloc.total, 0)
                   - COALESCE(ref_alloc.total, 0)
               )::numeric(14, 2) AS "available!: Decimal"
        FROM supplier_account_entries e
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount) AS total
            FROM supplier_account_allocations a
            WHERE a.entry_id = e.id
        ) alloc ON true
        LEFT JOIN LATERAL (
            SELECT SUM(ra.amount) AS total
            FROM supplier_account_credit_refund_allocations ra
            WHERE ra.source_entry_id = e.id
        ) ref_alloc ON true
        WHERE e.supplier_id = $1
          AND e.kind IN ('payment', 'purchase_return_credit')
          AND -e.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
        ORDER BY e.created_at, e.id
        "#,
        supplier_id
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut ids = Vec::new();
    let mut entry_ids = Vec::new();
    let mut amounts = Vec::new();
    let mut applied = Decimal::ZERO;

    for payment in payments {
        if remaining.is_zero() {
            break;
        }
        let take = remaining.min(payment.available);
        ensure_cents(take)?;
        ids.push(Uuid::now_v7());
        entry_ids.push(payment.entry_id);
        amounts.push(take);
        applied += take;
        remaining -= take;
    }

    if !remaining.is_zero() {
        return Err(RepoError::CreditApplicationBroken);
    }

    sqlx::query!(
        r#"
        INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount)
        SELECT id, entry_id, $3, amount
        FROM UNNEST($1::uuid[], $2::uuid[], $4::numeric(14, 2)[])
            AS planned(id, entry_id, amount)
        "#,
        &ids,
        &entry_ids,
        purchase_id,
        &amounts,
    )
    .execute(&mut *conn)
    .await?;

    Ok(applied)
}

/// One payment or return credit that still carries unallocated and
/// unrefunded money, and how much of it the next allocation or refund may
/// consume.
struct UnappliedSource {
    entry_id: Uuid,
    available: Decimal,
}

/// Money is only handed back against money the supplier actually holds for
/// the business: the unapplied credit says it was paid, the negative
/// balance says it was never spent on payables. Either alone could lie —
/// unapplied credit next to a payable, or a negative balance with the
/// credit already refunded — so the refund sees the smaller of the two,
/// floored at zero because a supplier we owe has nothing to hand back at
/// all.
fn refundable_credit(position: &SupplierAccountPosition) -> Decimal {
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
        SELECT id FROM supplier_account_entries
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
    refund: &NewSupplierCreditRefund<'_>,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries
            (id, supplier_id, kind, amount, wallet_movement_id, idempotency_key, note, created_by)
        VALUES ($1, $2, 'credit_refund', $3, $4, $5, $6, $7)
        "#,
        id,
        refund.supplier_id,
        refund.amount,
        refund.wallet_movement_id,
        refund.idempotency_key,
        refund.note,
        refund.created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("supplier_account_entries_one_entry_per_movement")
        | Some("supplier_account_entries_idempotency") => RepoError::AlreadyRecorded,
        _ => RepoError::Database(err),
    })?;

    Ok(())
}

/// Attribute the refunded credit to the advance payments or return credits
/// that hold unapplied credit: prefer entries backed by the same wallet
/// first, then oldest-first across remaining sources.
async fn unapplied_sources_wallet_first(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    wallet_id: Uuid,
) -> Result<Vec<UnappliedSource>, RepoError> {
    let sources = sqlx::query!(
        r#"
        SELECT e.id AS "entry_id!: Uuid",
               (
                   -e.amount
                   - COALESCE(alloc.total, 0)
                   - COALESCE(ref_alloc.total, 0)
               )::numeric(14, 2) AS "available!: Decimal"
        FROM supplier_account_entries e
        LEFT JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        LEFT JOIN LATERAL (
            SELECT SUM(a.amount) AS total
            FROM supplier_account_allocations a
            WHERE a.entry_id = e.id
        ) alloc ON true
        LEFT JOIN LATERAL (
            SELECT SUM(ra.amount) AS total
            FROM supplier_account_credit_refund_allocations ra
            WHERE ra.source_entry_id = e.id
        ) ref_alloc ON true
        WHERE e.supplier_id = $1
          AND e.kind IN ('payment', 'purchase_return_credit')
          AND -e.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
        ORDER BY CASE WHEN wm.wallet_id = $2 THEN 0 ELSE 1 END, e.created_at, e.id
        "#,
        supplier_id,
        wallet_id,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(sources
        .into_iter()
        .map(|source| UnappliedSource {
            entry_id: source.entry_id,
            available: source.available,
        })
        .collect())
}

/// Attribute the refund across the sources that hold it, one row per source
/// consumed, until the whole refund is placed. A remainder nobody could
/// cover means the caps derived from the locked snapshot disagreed with the
/// ledger's own invariants.
async fn attribute_refund_to_sources(
    conn: &mut PgConnection,
    refund_entry_id: Uuid,
    sources: Vec<UnappliedSource>,
    amount: Decimal,
) -> Result<(), RepoError> {
    let mut remaining = amount;
    for source in sources {
        if remaining.is_zero() {
            break;
        }
        let take = remaining.min(source.available);
        sqlx::query!(
            r#"
            INSERT INTO supplier_account_credit_refund_allocations
                (id, refund_entry_id, source_entry_id, amount)
            VALUES ($1, $2, $3, $4)
            "#,
            Uuid::now_v7(),
            refund_entry_id,
            source.entry_id,
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

/// Record cash returned by a supplier against unapplied advance/credit. Stored
/// positive because returning our credit moves the balance toward zero.
pub async fn record_credit_refund(
    conn: &mut PgConnection,
    refund: NewSupplierCreditRefund<'_>,
) -> Result<Uuid, RepoError> {
    ensure_cents(refund.amount)?;
    lock_supplier(conn, refund.supplier_id).await?;

    let refund_wallet_id = ensure_movement_backing(
        conn,
        refund.supplier_id,
        refund.wallet_movement_id,
        refund.amount,
        "supplier_account_refund",
    )
    .await?;

    ensure_refund_not_recorded(conn, refund.wallet_movement_id).await?;

    let position = account_position_in(conn, refund.supplier_id).await?;
    let available = refundable_credit(&position);
    if refund.amount > available {
        return Err(RepoError::CreditRefundExceedsCredit {
            requested: refund.amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    insert_credit_refund_entry(conn, id, &refund).await?;

    let sources =
        unapplied_sources_wallet_first(conn, refund.supplier_id, refund_wallet_id).await?;
    attribute_refund_to_sources(conn, id, sources, refund.amount).await?;

    Ok(id)
}

/// Record a manual payable adjustment (opening balance or correction). Must
/// carry a non-blank note, never cross zero with `adjustment_out`, and never
/// bury refundable advance credit (`unapplied_credit`) with `adjustment_in`.
pub async fn record_adjustment(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    kind: SupplierAccountEntryKind,
    amount: Decimal,
    note: &str,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    if !kind.is_manual() {
        return Err(RepoError::NotAnAdjustment);
    }
    if note.trim().is_empty() {
        return Err(RepoError::AdjustmentNoteRequired);
    }

    ensure_cents(amount)?;
    lock_supplier(conn, supplier_id).await?;

    let position = account_position_in(conn, supplier_id).await?;

    // When `balance >= 0`, `AdjustmentIn` adds manual payable freely and
    // `AdjustmentOut` cannot cross zero (`amount <= balance`).
    // When `balance < 0`, `AdjustmentOut` is always refused, while
    // `AdjustmentIn` is permitted up to the non-credit negative drift
    // `(-balance - unapplied_credit).max(0)` so operators can reconcile an
    // over-adjusted account back to zero without burying refundable advance
    // credit.
    let rejected = if position.balance < Decimal::ZERO {
        match kind {
            SupplierAccountEntryKind::AdjustmentIn => {
                let non_credit_drift = (-position.balance)
                    .checked_sub(position.unapplied_credit)
                    .unwrap_or(Decimal::ZERO)
                    .max(Decimal::ZERO);
                amount > non_credit_drift
            }
            _ => true,
        }
    } else {
        kind == SupplierAccountEntryKind::AdjustmentOut && amount > position.balance
    };

    if rejected {
        return Err(RepoError::AdjustmentCrossesZero {
            balance: position.balance,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, note, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        id,
        supplier_id,
        kind as SupplierAccountEntryKind,
        amount * Decimal::from(kind.sign()),
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await?;

    Ok(id)
}

/// Cancel open debt on a completed purchase from a purchase return.
///
/// The caller must already hold the supplier advisory lock. Enforces that
/// `amount` does not exceed the purchase's remaining open payable and writes
/// one negative `purchase_return` entry linked to both `purchase_id` and
/// `purchase_return_id`.
pub async fn record_purchase_return(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_id: Uuid,
    purchase_return_id: Uuid,
    amount: Decimal,
    note: Option<&str>,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_supplier(conn, supplier_id).await?;
    ensure_chargeable_purchase(conn, supplier_id, purchase_id).await?;

    // Mirror customer_accounts::record_credit_note: the return document itself
    // decides which purchase it belongs to and the maximum amount it carries.
    let ret = sqlx::query!(
        r#"
        SELECT purchase_id, total::numeric(14, 2) AS "total!: Decimal"
        FROM purchase_returns
        WHERE id = $1
        "#,
        purchase_return_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::PurchaseReturnNotFound)?;

    if ret.purchase_id != purchase_id {
        return Err(RepoError::PurchaseReturnMismatch);
    }
    if amount > ret.total {
        return Err(RepoError::PurchaseReturnExceeded {
            requested: amount,
            available: ret.total,
        });
    }

    let outstanding = outstanding_for_purchase_in(conn, purchase_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries
            (id, supplier_id, kind, amount, purchase_id, purchase_return_id, note, created_by)
        VALUES ($1, $2, 'purchase_return', $3, $4, $5, $6, $7)
        "#,
        id,
        supplier_id,
        -amount,
        purchase_id,
        purchase_return_id,
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err)
            == Some("supplier_account_entries_one_return_cancel_per_return")
        {
            RepoError::AlreadyRecorded
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Record the paid surplus of a purchase return as unapplied advance credit
/// with the supplier.
///
/// The caller must already hold the supplier advisory lock (and verify the
/// supplier is active). Writes one negative `purchase_return_credit` entry
/// linked to `purchase_return_id` with `purchase_id = NULL`, increasing
/// `unapplied_credit` by `amount` and decreasing `balance` by `amount`.
pub async fn record_purchase_return_credit(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_return_id: Uuid,
    amount: Decimal,
    note: Option<&str>,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_supplier(conn, supplier_id).await?;

    let ret = sqlx::query!(
        r#"
        SELECT p.supplier_id, r.total::numeric(14, 2) AS "total!: Decimal"
        FROM purchase_returns r
        JOIN purchases p ON p.id = r.purchase_id
        WHERE r.id = $1
        "#,
        purchase_return_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::PurchaseReturnNotFound)?;

    if ret.supplier_id != Some(supplier_id) {
        return Err(RepoError::PurchaseOfAnotherSupplier);
    }
    if amount > ret.total {
        return Err(RepoError::PurchaseReturnExceeded {
            requested: amount,
            available: ret.total,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO supplier_account_entries
            (id, supplier_id, kind, amount, purchase_return_id, note, created_by)
        VALUES ($1, $2, 'purchase_return_credit', $3, $4, $5, $6)
        "#,
        id,
        supplier_id,
        -amount,
        purchase_return_id,
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err)
            == Some("supplier_account_entries_one_return_credit_per_return")
        {
            RepoError::AlreadyRecorded
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}
