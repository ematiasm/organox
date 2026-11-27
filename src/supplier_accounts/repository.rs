//! Supplier accounts data access.
//!
//! Mirrors [`crate::customer_accounts::repository`]:
//! - `balance` is `SUM(amount)` over `supplier_account_entries` (positive = we
//!   owe the supplier, negative = advance credit in our favour);
//! - `outstanding(purchase)` is `purchase_charge - purchase_return - SUM(allocations)`;
//! - `unapplied_credit` is `(payment + purchase_return_credit) - refunded - allocated >= 0`.
//!
//! Concurrency: every balance-changing writer calls [`lock_supplier`] first,
//! acquiring `pg_advisory_xact_lock(hashtextextended(supplier_id, 1))` with a
//! 5-second local lock timeout before locking any `suppliers` row `FOR UPDATE`.
//! Using seed `1` keeps the advisory lock namespace disjoint from
//! `customer_accounts` (which uses seed `0`).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::model::{
    AgingBuckets, PayableSupplier, PayablesTotals, PurchaseOutstanding, SupplierAccountEntry,
    SupplierAccountEntryKind, SupplierAccountPosition, SupplierAdjustmentLogItem,
    SupplierPaymentLogAllocation, SupplierPaymentLogItem,
};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error("supplier not found")]
    SupplierNotFound,

    #[error("purchase not found")]
    PurchaseNotFound,

    #[error("account entry not found")]
    EntryNotFound,

    #[error("that purchase belongs to another supplier")]
    PurchaseOfAnotherSupplier,

    #[error("that purchase is not completed")]
    PurchaseNotChargeable,

    #[error("this purchase is already charged to the account")]
    AlreadyCharged,

    #[error("the amount must be positive")]
    NonPositiveAmount,

    #[error("the amount {amount} carries more than two decimals")]
    TooManyDecimals { amount: Decimal },

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("the allocation of {requested} exceeds the {outstanding} this purchase still owes")]
    OutstandingExceeded {
        requested: Decimal,
        outstanding: Decimal,
    },

    #[error("the allocations ({allocated}) exceed the money paid ({paid})")]
    PaymentOverAllocated { allocated: Decimal, paid: Decimal },

    #[error("this payment paid {paid} and {allocated} is already allocated")]
    AllocationExceedsPayment { paid: Decimal, allocated: Decimal },

    #[error("the allocation of {requested} exceeds the {available} held in supplier credit")]
    AllocationExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("only a payment can be allocated to a purchase")]
    NotAPayment,

    #[error("wallet movement not found")]
    MovementNotFound,

    #[error("the entry records {entry} but the wallet movement carries {movement}")]
    EntryDoesNotMatchMovement { movement: Decimal, entry: Decimal },

    #[error("the refund of {requested} exceeds the {available} held in supplier credit")]
    CreditRefundExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("an adjustment cannot cross zero: the balance is {balance}")]
    AdjustmentCrossesZero { balance: Decimal },

    #[error("that kind is not an adjustment")]
    NotAnAdjustment,

    #[error("an adjustment needs a reason")]
    AdjustmentNoteRequired,

    #[error("that document is already recorded in the ledger")]
    AlreadyRecorded,

    #[error("the charge of {requested} exceeds the {available} the purchase leaves unpaid")]
    ChargeExceedsPurchaseTotal {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the same purchase appears twice in one payment")]
    DuplicateAllocation { purchase_id: Uuid },

    #[error("a supplier account entry cannot be backed by a {kind} movement")]
    MovementNotFromSupplierAccount { kind: String },

    #[error("purchase return not found")]
    PurchaseReturnNotFound,

    #[error("that purchase return belongs to another purchase")]
    PurchaseReturnMismatch,

    #[error("the return of {requested} exceeds the {available} this purchase return carries")]
    PurchaseReturnExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the wallet movement belongs to another supplier")]
    MovementOfAnotherSupplier,

    #[error("the supplier account is busy, retry")]
    SupplierBusy,

    #[error("the supplier is deactivated")]
    SupplierInactive,

    #[error("the account would reach {requested} and only {available} is available")]
    CreditLimitExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the credit application disagreed with the ledger's own guards")]
    CreditApplicationBroken,
}

// --- Reads --------------------------------------------------------------

/// The supplier's account position as of now.
pub async fn account_position(
    pool: &PgPool,
    supplier_id: Uuid,
) -> Result<SupplierAccountPosition, RepoError> {
    let mut conn = pool.acquire().await?;
    account_position_in(&mut conn, supplier_id).await
}

/// [`account_position`] inside an open transaction (single consistent query).
pub async fn account_position_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
) -> Result<SupplierAccountPosition, RepoError> {
    let row = sqlx::query!(
        r#"
        WITH entries AS (
            SELECT COALESCE(SUM(amount), 0) AS balance,
                   COALESCE(SUM(CASE WHEN kind IN ('payment', 'purchase_return_credit')
                                     THEN -amount ELSE 0 END), 0)
                       AS paid,
                   COALESCE(SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END), 0)
                       AS refunded,
                   COALESCE(SUM(CASE WHEN kind IN ('adjustment_in', 'adjustment_out')
                                     THEN amount ELSE 0 END), 0)
                       AS adjustments
            FROM supplier_account_entries
            WHERE supplier_id = $1
        ),
        allocated AS (
            SELECT COALESCE(SUM(a.amount), 0) AS total
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            WHERE e.supplier_id = $1
        ),
        charges AS (
            SELECT purchase_id, SUM(amount) AS charged
            FROM supplier_account_entries
            WHERE supplier_id = $1 AND kind = 'purchase_charge'
            GROUP BY purchase_id
        ),
        cancellations AS (
            SELECT purchase_id, SUM(-amount) AS cancelled
            FROM supplier_account_entries
            WHERE supplier_id = $1 AND kind = 'purchase_return'
            GROUP BY purchase_id
        ),
        purchase_allocated AS (
            SELECT a.purchase_id, SUM(a.amount) AS allocated
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            WHERE e.supplier_id = $1
            GROUP BY a.purchase_id
        ),
        payables AS (
            SELECT c.charged - COALESCE(r.cancelled, 0) - COALESCE(l.allocated, 0) AS outstanding
            FROM charges c
            LEFT JOIN cancellations r ON r.purchase_id = c.purchase_id
            LEFT JOIN purchase_allocated l ON l.purchase_id = c.purchase_id
        )
        SELECT balance::numeric(14, 2)                       AS "balance!: Decimal",
               (paid - refunded - allocated.total)::numeric(14, 2)
                                                             AS "unapplied_credit!: Decimal",
               adjustments::numeric(14, 2)                   AS "adjustments!: Decimal",
               COALESCE((SELECT SUM(outstanding) FROM payables WHERE outstanding > 0), 0)
                   ::numeric(14, 2)                          AS "outstanding_total!: Decimal"
        FROM entries, allocated
        "#,
        supplier_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(SupplierAccountPosition {
        balance: row.balance,
        unapplied_credit: row.unapplied_credit,
        outstanding_total: row.outstanding_total,
        adjustments: row.adjustments,
    })
}

/// Every completed purchase of this supplier with an open payable, oldest first.
pub async fn outstanding_by_purchase(
    pool: &PgPool,
    supplier_id: Uuid,
) -> Result<Vec<PurchaseOutstanding>, RepoError> {
    let mut conn = pool.acquire().await?;
    outstanding_by_purchase_in(&mut conn, supplier_id).await
}

/// [`outstanding_by_purchase`] inside an open transaction.
pub async fn outstanding_by_purchase_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
) -> Result<Vec<PurchaseOutstanding>, RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH charges AS (
            SELECT purchase_id, SUM(amount) AS charged
            FROM supplier_account_entries
            WHERE supplier_id = $1 AND kind = 'purchase_charge'
            GROUP BY purchase_id
        ),
        cancellations AS (
            SELECT purchase_id, SUM(-amount) AS cancelled
            FROM supplier_account_entries
            WHERE supplier_id = $1 AND kind = 'purchase_return'
            GROUP BY purchase_id
        ),
        allocated AS (
            SELECT a.purchase_id, SUM(a.amount) AS allocated
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            WHERE e.supplier_id = $1
            GROUP BY a.purchase_id
        )
        SELECT c.purchase_id                              AS "purchase_id!: Uuid",
               p.completed_at                             AS "completed_at!: DateTime<Utc>",
               c.charged::numeric(14, 2)                  AS "charged!: Decimal",
               COALESCE(r.cancelled, 0)::numeric(14, 2)   AS "cancelled!: Decimal",
               COALESCE(l.allocated, 0)::numeric(14, 2)   AS "allocated!: Decimal",
               (c.charged - COALESCE(r.cancelled, 0) - COALESCE(l.allocated, 0))::numeric(14, 2)
                                                          AS "outstanding!: Decimal"
        FROM charges c
        JOIN purchases p ON p.id = c.purchase_id
        LEFT JOIN cancellations r ON r.purchase_id = c.purchase_id
        LEFT JOIN allocated l ON l.purchase_id = c.purchase_id
        WHERE c.charged - COALESCE(r.cancelled, 0) - COALESCE(l.allocated, 0) > 0
        ORDER BY p.completed_at, c.purchase_id
        "#,
        supplier_id
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PurchaseOutstanding {
            purchase_id: row.purchase_id,
            completed_at: row.completed_at,
            charged: row.charged,
            cancelled: row.cancelled,
            allocated: row.allocated,
            outstanding: row.outstanding,
        })
        .collect())
}

/// Whether the supplier row exists. Read endpoints return 404 when false.
pub async fn supplier_exists_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
) -> Result<bool, RepoError> {
    let found = sqlx::query!(
        r#"SELECT 1 AS found FROM suppliers WHERE id = $1"#,
        supplier_id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(found.is_some())
}

/// A bounded window of ledger entries for a supplier, oldest first
/// (`created_at ASC, id ASC`), each carrying the running balance after it.
///
/// The running balance is a window over the whole ledger, not over the page:
/// `limit`/`offset` only choose which entries come back, so a later page still
/// reports the balance the account really had at that entry instead of
/// restarting from zero.
///
/// The `id` tie-breaker (UUIDv7) keeps the order deterministic when two entries
/// share the same database timestamp.
pub async fn list_entries_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<(SupplierAccountEntry, Decimal)>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT id,
               supplier_id,
               kind AS "kind: SupplierAccountEntryKind",
               amount,
               purchase_id,
               purchase_return_id,
               wallet_movement_id,
               note,
               created_by,
               created_at,
               (SUM(amount) OVER (
                   ORDER BY created_at ASC, id ASC
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
               ))::numeric AS "running_balance!: Decimal",
               COUNT(*) OVER () AS "entries_total!"
        FROM supplier_account_entries
        WHERE supplier_id = $1
        ORDER BY created_at ASC, id ASC
        LIMIT $2 OFFSET $3
        "#,
        supplier_id,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    let entries_total = rows.first().map(|row| row.entries_total).unwrap_or(-1);

    let entries = rows
        .into_iter()
        .map(|row| {
            (
                SupplierAccountEntry {
                    id: row.id,
                    supplier_id: row.supplier_id,
                    kind: row.kind,
                    amount: row.amount,
                    purchase_id: row.purchase_id,
                    purchase_return_id: row.purchase_return_id,
                    wallet_movement_id: row.wallet_movement_id,
                    note: row.note,
                    created_by: row.created_by,
                    created_at: row.created_at,
                },
                row.running_balance,
            )
        })
        .collect();

    Ok((entries, entries_total))
}

/// Total number of ledger entries for a supplier.
///
/// `list_entries_in` reports the count in each row, so this only runs when the
/// requested window is past the end and the page query returns nothing.
pub async fn count_entries_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
) -> Result<i64, RepoError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM supplier_account_entries WHERE supplier_id = $1"#,
        supplier_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(count)
}

/// Supplier payment write log (`kind = 'payment'`), newest first, with each
/// payment's linked wallet movement and allocations.
pub async fn list_payments_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<SupplierPaymentLogItem>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT e.id AS entry_id,
               e.wallet_movement_id AS "wallet_movement_id!: Uuid",
               wm.wallet_id,
               wm.payment_method_id,
               (-e.amount)::numeric(14, 2) AS "amount!: Decimal",
               wm.note,
               e.created_by,
               e.created_at
        FROM supplier_account_entries e
        JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        WHERE e.supplier_id = $1
          AND e.kind = 'payment'
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT $2
        OFFSET $3
        "#,
        supplier_id,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let entry_ids: Vec<Uuid> = rows.iter().map(|row| row.entry_id).collect();
    let alloc_rows = sqlx::query!(
        r#"
        SELECT entry_id, purchase_id, amount, created_at
        FROM supplier_account_allocations
        WHERE entry_id = ANY($1)
        ORDER BY created_at ASC, id ASC
        "#,
        &entry_ids
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut by_entry: HashMap<Uuid, Vec<SupplierPaymentLogAllocation>> = HashMap::new();
    for alloc in alloc_rows {
        by_entry
            .entry(alloc.entry_id)
            .or_default()
            .push(SupplierPaymentLogAllocation {
                purchase_id: alloc.purchase_id,
                amount: alloc.amount,
                created_at: alloc.created_at,
            });
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let allocations = by_entry.remove(&row.entry_id).unwrap_or_default();
            let allocated_total = allocations.iter().map(|a| a.amount).sum();
            SupplierPaymentLogItem {
                entry_id: row.entry_id,
                wallet_movement_id: row.wallet_movement_id,
                wallet_id: row.wallet_id,
                payment_method_id: row.payment_method_id,
                amount: row.amount,
                allocated_total,
                allocations,
                note: row.note,
                created_by: row.created_by,
                created_at: row.created_at,
            }
        })
        .collect())
}

/// Manual adjustment write log (`adjustment_in` / `adjustment_out`), newest
/// first.
pub async fn list_adjustments_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<SupplierAdjustmentLogItem>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT id AS entry_id,
               kind AS "kind: SupplierAccountEntryKind",
               amount,
               note AS "note!: String",
               created_by,
               created_at
        FROM supplier_account_entries
        WHERE supplier_id = $1
          AND kind IN ('adjustment_in', 'adjustment_out')
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        OFFSET $3
        "#,
        supplier_id,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SupplierAdjustmentLogItem {
            entry_id: row.entry_id,
            kind: row.kind,
            amount: row.amount,
            note: row.note,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect())
}

/// Portfolio-wide payables report: one row per supplier with an open position
/// (`balance != 0` or `unapplied_credit != 0`), plus totals over the whole
/// report.
///
/// `limit`/`offset` choose which suppliers come back; the totals are window
/// aggregates over the filtered set, so they describe every supplier with a
/// balance no matter which page the caller asked for.
///
/// Elapsed whole days `floor((now - completed_at) / 86400)` partition every
/// open purchase into exactly one bucket (`<= 30`, `31..=60`, `61..=90`,
/// `> 90`), so the four buckets always sum to `outstanding_total`.
pub async fn list_payables(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PayableSupplier>, PayablesTotals), RepoError> {
    let mut conn = pool.acquire().await?;
    list_payables_in(&mut conn, limit, offset).await
}

/// [`list_payables`] inside an open transaction (such as a `REPEATABLE READ,
/// READ ONLY` snapshot).
pub async fn list_payables_in(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PayableSupplier>, PayablesTotals), RepoError> {
    let (items, totals) = payables_page(conn, limit, offset).await?;

    // A window past the end has no rows to carry the aggregates, and answering
    // zeroed totals for a report that has suppliers would be a lie. Only
    // re-read when `offset > 0` (an empty first page proves the report is empty)
    // and bound the fallback to `LIMIT 1` so window totals come back in one row.
    if items.is_empty() && offset > 0 {
        let (_, totals) = payables_page(conn, 1, 0).await?;
        return Ok((Vec::new(), totals));
    }

    Ok((items, totals))
}

async fn payables_page(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PayableSupplier>, PayablesTotals), RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH entry_totals AS (
            SELECT
                supplier_id,
                COALESCE(SUM(amount), 0) AS balance,
                COALESCE(SUM(CASE WHEN kind IN ('payment', 'purchase_return_credit')
                                  THEN -amount ELSE 0 END), 0) AS paid,
                COALESCE(SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END), 0)
                    AS refunded,
                COALESCE(SUM(CASE WHEN kind IN ('adjustment_in', 'adjustment_out')
                                  THEN amount ELSE 0 END), 0) AS adjustments
            FROM supplier_account_entries
            GROUP BY supplier_id
        ),
        supplier_allocated AS (
            SELECT e.supplier_id, COALESCE(SUM(a.amount), 0) AS total
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            GROUP BY e.supplier_id
        ),
        charges AS (
            SELECT supplier_id, purchase_id, SUM(amount) AS charged
            FROM supplier_account_entries
            WHERE kind = 'purchase_charge'
            GROUP BY supplier_id, purchase_id
        ),
        cancellations AS (
            SELECT supplier_id, purchase_id, SUM(-amount) AS cancelled
            FROM supplier_account_entries
            WHERE kind = 'purchase_return'
            GROUP BY supplier_id, purchase_id
        ),
        purchase_allocated AS (
            SELECT e.supplier_id, a.purchase_id, SUM(a.amount) AS allocated
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            GROUP BY e.supplier_id, a.purchase_id
        ),
        open_purchases AS (
            SELECT
                ch.supplier_id,
                (ch.charged - COALESCE(cn.cancelled, 0) - COALESCE(pa.allocated, 0)) AS outstanding,
                FLOOR(EXTRACT(EPOCH FROM (NOW() - p.completed_at)) / 86400)::bigint AS age_days
            FROM charges ch
            JOIN purchases p ON p.id = ch.purchase_id
            LEFT JOIN cancellations cn
                   ON cn.supplier_id = ch.supplier_id AND cn.purchase_id = ch.purchase_id
            LEFT JOIN purchase_allocated pa
                   ON pa.supplier_id = ch.supplier_id AND pa.purchase_id = ch.purchase_id
            WHERE ch.charged - COALESCE(cn.cancelled, 0) - COALESCE(pa.allocated, 0) > 0
        ),
        aging_by_supplier AS (
            SELECT
                supplier_id,
                COALESCE(SUM(outstanding), 0) AS outstanding_total,
                COALESCE(SUM(CASE WHEN age_days <= 30 THEN outstanding ELSE 0 END), 0)
                    AS days_0_30,
                COALESCE(SUM(CASE WHEN age_days BETWEEN 31 AND 60 THEN outstanding ELSE 0 END), 0)
                    AS days_31_60,
                COALESCE(SUM(CASE WHEN age_days BETWEEN 61 AND 90 THEN outstanding ELSE 0 END), 0)
                    AS days_61_90,
                COALESCE(SUM(CASE WHEN age_days > 90 THEN outstanding ELSE 0 END), 0)
                    AS days_90_plus
            FROM open_purchases
            GROUP BY supplier_id
        )
        SELECT
            s.id AS "supplier_id!",
            s.name AS "supplier_name!",
            s.tax_id,
            s.is_active AS "is_active!",
            et.balance::numeric(14, 2) AS "balance!: Decimal",
            (et.paid - et.refunded - COALESCE(sa.total, 0))::numeric(14, 2)
                AS "unapplied_credit!: Decimal",
            COALESCE(ag.outstanding_total, 0)::numeric(14, 2) AS "outstanding_total!: Decimal",
            et.adjustments::numeric(14, 2) AS "adjustments!: Decimal",
            COALESCE(ag.days_0_30, 0)::numeric(14, 2) AS "days_0_30!: Decimal",
            COALESCE(ag.days_31_60, 0)::numeric(14, 2) AS "days_31_60!: Decimal",
            COALESCE(ag.days_61_90, 0)::numeric(14, 2) AS "days_61_90!: Decimal",
            COALESCE(ag.days_90_plus, 0)::numeric(14, 2) AS "days_90_plus!: Decimal",
            COUNT(*) OVER () AS "items_total!",
            (SUM(et.balance) OVER ())::numeric AS "total_balance!: Decimal",
            (SUM(et.paid - et.refunded - COALESCE(sa.total, 0)) OVER ())
                ::numeric AS "total_unapplied_credit!: Decimal",
            (SUM(COALESCE(ag.outstanding_total, 0)) OVER ())::numeric
                AS "total_outstanding!: Decimal",
            (SUM(et.adjustments) OVER ())::numeric AS "total_adjustments!: Decimal",
            (SUM(COALESCE(ag.days_0_30, 0)) OVER ())::numeric
                AS "aging_days_0_30!: Decimal",
            (SUM(COALESCE(ag.days_31_60, 0)) OVER ())::numeric
                AS "aging_days_31_60!: Decimal",
            (SUM(COALESCE(ag.days_61_90, 0)) OVER ())::numeric
                AS "aging_days_61_90!: Decimal",
            (SUM(COALESCE(ag.days_90_plus, 0)) OVER ())::numeric
                AS "aging_days_90_plus!: Decimal"
        FROM entry_totals et
        JOIN suppliers s ON s.id = et.supplier_id
        LEFT JOIN supplier_allocated sa ON sa.supplier_id = et.supplier_id
        LEFT JOIN aging_by_supplier ag ON ag.supplier_id = et.supplier_id
        WHERE et.balance <> 0
           OR (et.paid - et.refunded - COALESCE(sa.total, 0)) <> 0
        ORDER BY et.balance DESC, lower(s.name) ASC, s.id ASC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    let Some(first) = rows.first() else {
        return Ok((Vec::new(), PayablesTotals::default()));
    };

    let totals = PayablesTotals {
        items_total: first.items_total,
        total_balance: first.total_balance,
        total_unapplied_credit: first.total_unapplied_credit,
        total_outstanding: first.total_outstanding,
        total_adjustments: first.total_adjustments,
        aging: AgingBuckets {
            days_0_30: first.aging_days_0_30,
            days_31_60: first.aging_days_31_60,
            days_61_90: first.aging_days_61_90,
            days_90_plus: first.aging_days_90_plus,
        },
    };

    let items = rows
        .into_iter()
        .map(|row| PayableSupplier {
            supplier_id: row.supplier_id,
            supplier_name: row.supplier_name,
            tax_id: row.tax_id,
            is_active: row.is_active,
            balance: row.balance,
            unapplied_credit: row.unapplied_credit,
            outstanding_total: row.outstanding_total,
            adjustments: row.adjustments,
            aging: AgingBuckets {
                days_0_30: row.days_0_30,
                days_31_60: row.days_31_60,
                days_61_90: row.days_61_90,
                days_90_plus: row.days_90_plus,
            },
        })
        .collect();

    Ok((items, totals))
}

// --- Writes -------------------------------------------------------------

fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}

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

/// What one purchase still owes on account (`charge - cancelled - allocated`).
pub async fn outstanding_for_purchase_in(
    conn: &mut PgConnection,
    purchase_id: Uuid,
) -> Result<Decimal, RepoError> {
    let outstanding = sqlx::query_scalar!(
        r#"
        WITH charge AS (
            SELECT COALESCE(SUM(amount), 0) AS charged
            FROM supplier_account_entries
            WHERE purchase_id = $1 AND kind = 'purchase_charge'
        ),
        cancelled AS (
            SELECT COALESCE(-SUM(amount), 0) AS cancelled
            FROM supplier_account_entries
            WHERE purchase_id = $1 AND kind = 'purchase_return'
        ),
        allocated AS (
            SELECT COALESCE(SUM(amount), 0) AS allocated
            FROM supplier_account_allocations
            WHERE purchase_id = $1
        )
        SELECT (charge.charged - cancelled.cancelled - allocated.allocated)::numeric(14, 2)
               AS "outstanding!: Decimal"
        FROM charge, cancelled, allocated
        "#,
        purchase_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(outstanding)
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

/// A supplier payment: the wallet outflow that moved the money, and the
/// completed purchases the operator chose to allocate it to.
pub struct NewSupplierPayment<'a> {
    pub supplier_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}

pub struct RecordedSupplierPayment {
    pub entry_id: Uuid,
    pub replayed: bool,
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

    let mut seen = HashSet::new();
    for (purchase_id, _) in payment.allocations {
        if !seen.insert(*purchase_id) {
            return Err(RepoError::DuplicateAllocation {
                purchase_id: *purchase_id,
            });
        }
    }

    if let Some(key) = payment.idempotency_key {
        let first = sqlx::query_scalar!(
            r#"
            SELECT id FROM supplier_account_entries
            WHERE supplier_id = $1 AND idempotency_key = $2 AND kind = 'payment'
            "#,
            payment.supplier_id,
            key,
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(entry_id) = first {
            return Ok(RecordedSupplierPayment {
                entry_id,
                replayed: true,
            });
        }
    }

    let mut allocated = Decimal::ZERO;
    for (purchase_id, amount) in payment.allocations {
        ensure_cents(*amount)?;
        ensure_chargeable_purchase(conn, payment.supplier_id, *purchase_id).await?;

        let outstanding = outstanding_for_purchase_in(conn, *purchase_id).await?;
        if *amount > outstanding {
            return Err(RepoError::OutstandingExceeded {
                requested: *amount,
                outstanding,
            });
        }
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

    let entry_id = Uuid::now_v7();
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

    for (purchase_id, amount) in payment.allocations {
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

    Ok(RecordedSupplierPayment {
        entry_id,
        replayed: false,
    })
}

/// An already-recorded supplier payment for idempotent replay.
pub struct ExistingSupplierPayment {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub purchase_ids: Vec<Uuid>,
}

/// Look up an existing supplier payment by `(supplier_id, idempotency_key)`
/// under the supplier lock before writing a wallet movement.
pub async fn payment_by_key_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<ExistingSupplierPayment>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT e.id                                AS "entry_id!: Uuid",
               e.wallet_movement_id                AS "wallet_movement_id!: Uuid",
               COALESCE(
                   ARRAY_REMOVE(ARRAY_AGG(a.purchase_id ORDER BY a.created_at, a.purchase_id), NULL),
                   '{}'
               )                                   AS "purchase_ids!: Vec<Uuid>"
        FROM supplier_account_entries e
        LEFT JOIN supplier_account_allocations a ON a.entry_id = e.id
        WHERE e.supplier_id = $1 AND e.idempotency_key = $2 AND e.kind = 'payment'
        GROUP BY e.id, e.wallet_movement_id
        "#,
        supplier_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| ExistingSupplierPayment {
        entry_id: row.entry_id,
        wallet_movement_id: row.wallet_movement_id,
        purchase_ids: row.purchase_ids,
    }))
}

/// Allocate already-paid supplier money to one completed purchase.
pub async fn allocate(
    conn: &mut PgConnection,
    entry_id: Uuid,
    purchase_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    ensure_cents(amount)?;

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

    lock_supplier(conn, entry.supplier_id).await?;

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

    let paid = -entry.amount;
    if already.checked_add(amount).is_none_or(|sum| sum > paid) {
        return Err(RepoError::AllocationExceedsPayment {
            paid,
            allocated: already,
        });
    }

    ensure_chargeable_purchase(conn, entry.supplier_id, purchase_id).await?;

    let outstanding = outstanding_for_purchase_in(conn, purchase_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

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

    let position = account_position_in(conn, entry.supplier_id).await?;
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
    .await?;

    Ok(())
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

/// An already-recorded supplier credit refund for idempotent replay.
pub struct ExistingSupplierCreditRefund {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
}

/// Look up an existing credit refund by `(supplier_id, idempotency_key)` under
/// the supplier lock before writing anything.
pub async fn credit_refund_by_key_in(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<ExistingSupplierCreditRefund>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id                 AS "entry_id!: Uuid",
               wallet_movement_id AS "wallet_movement_id!: Uuid"
        FROM supplier_account_entries
        WHERE supplier_id = $1 AND idempotency_key = $2 AND kind = 'credit_refund'
        "#,
        supplier_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| ExistingSupplierCreditRefund {
        entry_id: row.entry_id,
        wallet_movement_id: row.wallet_movement_id,
    }))
}

pub struct NewSupplierCreditRefund<'a> {
    pub supplier_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
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

    let already = sqlx::query_scalar!(
        r#"
        SELECT id FROM supplier_account_entries
        WHERE wallet_movement_id = $1
        "#,
        refund.wallet_movement_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if already.is_some() {
        return Err(RepoError::AlreadyRecorded);
    }

    let position = account_position_in(conn, refund.supplier_id).await?;

    let available = position
        .unapplied_credit
        .min(-position.balance)
        .max(Decimal::ZERO);
    if refund.amount > available {
        return Err(RepoError::CreditRefundExceedsCredit {
            requested: refund.amount,
            available,
        });
    }

    let id = Uuid::now_v7();
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

    // Attribute the refunded credit to the advance payments or return credits
    // that hold unapplied credit: prefer entries backed by the same wallet
    // first, then oldest-first across remaining sources.
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
        refund.supplier_id,
        refund_wallet_id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut remaining = refund.amount;
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
            id,
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
