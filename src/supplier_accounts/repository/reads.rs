//! Every read of the supplier account ledger: the position queries, the
//! entry and payment logs, the portfolio-wide payables report, and the
//! replay lookups a writer consults under the supplier lock before it
//! writes anything.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use std::collections::HashMap;
use uuid::Uuid;

use super::{ExistingSupplierCreditRefund, ExistingSupplierPayment, RepoError};
use crate::supplier_accounts::model::{
    AgingBuckets, PayableSupplier, PayablesTotals, PurchaseOutstanding, SupplierAccountEntry,
    SupplierAccountEntryKind, SupplierAccountPosition, SupplierAdjustmentLogItem,
    SupplierPaymentLogAllocation, SupplierPaymentLogItem,
};

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
