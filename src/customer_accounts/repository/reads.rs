//! Every read of the customer account ledger: the position queries, the
//! entry and collection logs, the portfolio-wide receivables report, and
//! the replay lookups a writer consults under the customer lock before it
//! writes anything.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use std::collections::HashMap;
use uuid::Uuid;

use super::{ExistingCreditRefund, ExistingReceipt, RepoError};
use crate::customer_accounts::model::{
    AccountPosition, AdjustmentLogItem, AgingBuckets, CustomerAccountEntry,
    CustomerAccountEntryKind, ReceiptLogAllocation, ReceiptLogItem, ReceivableCustomer,
    ReceivablesTotals, SaleOutstanding,
};

/// The customer's account as of now: balance, usable credit, what is owed.
pub async fn account_position(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<AccountPosition, RepoError> {
    let mut conn = pool.acquire().await?;
    account_position_in(&mut conn, customer_id).await
}

/// [`account_position`] inside an open transaction.
///
/// One statement, one snapshot: the aggregates and `outstanding_total` are
/// read at the same moment of the ledger, so a position read outside the
/// customer lock can no longer show a balance from one instant and a debt
/// total from the next. The scalar subquery repeats the definition
/// [`outstanding_by_sale_in`] lists — per sale, charge minus cancelled minus
/// allocated, positive rows only — so the two surfaces cannot disagree.
pub async fn account_position_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<AccountPosition, RepoError> {
    let row = sqlx::query!(
        r#"
        WITH entries AS (
            SELECT COALESCE(SUM(amount), 0) AS balance,
                   COALESCE(SUM(CASE WHEN kind = 'receipt' THEN -amount ELSE 0 END), 0)
                       AS received,
                   COALESCE(SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END), 0)
                       AS refunded,
                   COALESCE(SUM(CASE WHEN kind IN ('adjustment_in', 'adjustment_out')
                                     THEN amount ELSE 0 END), 0)
                       AS adjustments
            FROM customer_account_entries
            WHERE customer_id = $1
        ),
        allocated AS (
            SELECT COALESCE(SUM(a.amount), 0) AS total
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
        ),
        charges AS (
            SELECT sale_id, SUM(amount) AS charged
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'sale_charge'
            GROUP BY sale_id
        ),
        cancelled AS (
            SELECT sale_id, -SUM(amount) AS cancelled
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'credit_note'
            GROUP BY sale_id
        ),
        sale_allocated AS (
            SELECT a.sale_id, SUM(a.amount) AS allocated
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
            GROUP BY a.sale_id
        ),
        debts AS (
            SELECT c.charged - COALESCE(x.cancelled, 0) - COALESCE(l.allocated, 0)
                AS outstanding
            FROM charges c
            LEFT JOIN cancelled x ON x.sale_id = c.sale_id
            LEFT JOIN sale_allocated l ON l.sale_id = c.sale_id
        )
        SELECT balance::numeric(14, 2)                       AS "balance!: Decimal",
               (received - refunded - allocated.total)::numeric(14, 2)
                                                             AS "unapplied_credit!: Decimal",
               adjustments::numeric(14, 2)                   AS "adjustments!: Decimal",
               COALESCE((SELECT SUM(outstanding) FROM debts WHERE outstanding > 0), 0)
                   ::numeric(14, 2)                          AS "outstanding_total!: Decimal"
        FROM entries, allocated
        "#,
        customer_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(AccountPosition {
        balance: row.balance,
        unapplied_credit: row.unapplied_credit,
        outstanding_total: row.outstanding_total,
        adjustments: row.adjustments,
    })
}

/// Every sale of this customer that still owes money, oldest first.
pub async fn outstanding_by_sale(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<Vec<SaleOutstanding>, RepoError> {
    let mut conn = pool.acquire().await?;
    outstanding_by_sale_in(&mut conn, customer_id).await
}

/// [`outstanding_by_sale`] inside an open transaction.
pub async fn outstanding_by_sale_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<Vec<SaleOutstanding>, RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH charges AS (
            SELECT sale_id, SUM(amount) AS charged
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'sale_charge'
            GROUP BY sale_id
        ),
        cancelled AS (
            SELECT sale_id, -SUM(amount) AS cancelled
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'credit_note'
            GROUP BY sale_id
        ),
        allocated AS (
            SELECT a.sale_id, SUM(a.amount) AS allocated
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
            GROUP BY a.sale_id
        )
        SELECT c.sale_id                             AS "sale_id!: Uuid",
               s.completed_at                        AS "completed_at!: DateTime<Utc>",
               c.charged::numeric(14, 2)             AS "charged!: Decimal",
               COALESCE(x.cancelled, 0)::numeric(14, 2) AS "cancelled!: Decimal",
               COALESCE(l.allocated, 0)::numeric(14, 2) AS "allocated!: Decimal",
               (c.charged
                    - COALESCE(x.cancelled, 0)
                    - COALESCE(l.allocated, 0))::numeric(14, 2) AS "outstanding!: Decimal"
        FROM charges c
        JOIN sales s ON s.id = c.sale_id
        LEFT JOIN cancelled x ON x.sale_id = c.sale_id
        LEFT JOIN allocated l ON l.sale_id = c.sale_id
        WHERE c.charged - COALESCE(x.cancelled, 0) - COALESCE(l.allocated, 0) > 0
        ORDER BY s.completed_at, c.sale_id
        "#,
        customer_id
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SaleOutstanding {
            sale_id: row.sale_id,
            completed_at: row.completed_at,
            charged: row.charged,
            cancelled: row.cancelled,
            allocated: row.allocated,
            outstanding: row.outstanding,
        })
        .collect())
}

/// Whether the customer row exists. Read endpoints answer `404` when it does
/// not, without taking any write lock.
pub async fn customer_exists_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<bool, RepoError> {
    let found = sqlx::query!(
        r#"SELECT 1 AS found FROM customers WHERE id = $1"#,
        customer_id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(found.is_some())
}

/// A bounded window of ledger entries for a customer, oldest first
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
    customer_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<(CustomerAccountEntry, Decimal)>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT id,
               customer_id,
               kind AS "kind: CustomerAccountEntryKind",
               amount,
               sale_id,
               credit_note_id,
               wallet_movement_id,
               note,
               created_by,
               created_at,
               (SUM(amount) OVER (
                   ORDER BY created_at ASC, id ASC
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
               ))::numeric AS "running_balance!: Decimal",
               COUNT(*) OVER () AS "entries_total!"
        FROM customer_account_entries
        WHERE customer_id = $1
        ORDER BY created_at ASC, id ASC
        LIMIT $2 OFFSET $3
        "#,
        customer_id,
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
                CustomerAccountEntry {
                    id: row.id,
                    customer_id: row.customer_id,
                    kind: row.kind,
                    amount: row.amount,
                    sale_id: row.sale_id,
                    credit_note_id: row.credit_note_id,
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

/// Total number of ledger entries for a customer.
///
/// `list_entries_in` reports the count in each row, so this only runs when the
/// requested window is past the end and the page query returns nothing.
pub async fn count_entries_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<i64, RepoError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM customer_account_entries WHERE customer_id = $1"#,
        customer_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(count)
}

/// Collection write log (`kind = 'receipt'`), newest first, with each
/// receipt's linked wallet movement and allocations.
pub async fn list_receipts_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReceiptLogItem>, RepoError> {
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
        FROM customer_account_entries e
        JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        WHERE e.customer_id = $1
          AND e.kind = 'receipt'
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT $2
        OFFSET $3
        "#,
        customer_id,
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
        SELECT entry_id, sale_id, amount, created_at
        FROM customer_account_allocations
        WHERE entry_id = ANY($1)
        ORDER BY created_at ASC, id ASC
        "#,
        &entry_ids
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut by_entry: HashMap<Uuid, Vec<ReceiptLogAllocation>> = HashMap::new();
    for alloc in alloc_rows {
        by_entry
            .entry(alloc.entry_id)
            .or_default()
            .push(ReceiptLogAllocation {
                sale_id: alloc.sale_id,
                amount: alloc.amount,
                created_at: alloc.created_at,
            });
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let allocations = by_entry.remove(&row.entry_id).unwrap_or_default();
            let allocated_total = allocations.iter().map(|a| a.amount).sum();
            ReceiptLogItem {
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
    customer_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdjustmentLogItem>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT id AS entry_id,
               kind AS "kind: CustomerAccountEntryKind",
               amount,
               note AS "note!: String",
               created_by,
               created_at
        FROM customer_account_entries
        WHERE customer_id = $1
          AND kind IN ('adjustment_in', 'adjustment_out')
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        OFFSET $3
        "#,
        customer_id,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| AdjustmentLogItem {
            entry_id: row.entry_id,
            kind: row.kind,
            amount: row.amount,
            note: row.note,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect())
}

/// Portfolio-wide receivables report: one row per customer with an open
/// position (`balance != 0` or `unapplied_credit != 0`), plus totals over the
/// whole report.
///
/// `limit`/`offset` choose which customers come back; the totals are window
/// aggregates over the filtered set, so they describe every customer with a
/// balance no matter which page the caller asked for.
///
/// Elapsed whole days `floor((now - completed_at) / 86400)` partition every
/// open sale into exactly one bucket (`<= 30`, `31..=60`, `61..=90`, `> 90`),
/// so the four buckets always sum to `outstanding_total`.
pub async fn list_receivables(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReceivableCustomer>, ReceivablesTotals), RepoError> {
    let mut conn = pool.acquire().await?;
    list_receivables_in(&mut conn, limit, offset).await
}

/// [`list_receivables`] inside an open transaction (such as a `REPEATABLE READ,
/// READ ONLY` snapshot).
pub async fn list_receivables_in(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReceivableCustomer>, ReceivablesTotals), RepoError> {
    let (items, totals) = receivables_page(conn, limit, offset).await?;

    // A window past the end has no rows to carry the aggregates, and answering
    // zeroed totals for a report that has customers would be a lie. Only
    // re-read when `offset > 0` (an empty first page proves the report is empty)
    // and bound the fallback to `LIMIT 1` so window totals come back in one row.
    if items.is_empty() && offset > 0 {
        let (_, totals) = receivables_page(conn, 1, 0).await?;
        return Ok((Vec::new(), totals));
    }

    Ok((items, totals))
}

async fn receivables_page(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReceivableCustomer>, ReceivablesTotals), RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH entry_totals AS (
            SELECT
                customer_id,
                COALESCE(SUM(amount), 0) AS balance,
                COALESCE(SUM(CASE WHEN kind = 'receipt' THEN -amount ELSE 0 END), 0) AS received,
                COALESCE(SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END), 0)
                    AS refunded,
                COALESCE(SUM(CASE WHEN kind IN ('adjustment_in', 'adjustment_out')
                                  THEN amount ELSE 0 END), 0) AS adjustments
            FROM customer_account_entries
            GROUP BY customer_id
        ),
        customer_allocated AS (
            SELECT e.customer_id, COALESCE(SUM(a.amount), 0) AS total
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            GROUP BY e.customer_id
        ),
        charges AS (
            SELECT customer_id, sale_id, SUM(amount) AS charged
            FROM customer_account_entries
            WHERE kind = 'sale_charge'
            GROUP BY customer_id, sale_id
        ),
        cancelled AS (
            SELECT customer_id, sale_id, -SUM(amount) AS cancelled
            FROM customer_account_entries
            WHERE kind = 'credit_note'
            GROUP BY customer_id, sale_id
        ),
        sale_allocated AS (
            SELECT e.customer_id, a.sale_id, SUM(a.amount) AS allocated
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            GROUP BY e.customer_id, a.sale_id
        ),
        open_sales AS (
            SELECT
                ch.customer_id,
                (ch.charged - COALESCE(cx.cancelled, 0) - COALESCE(sa.allocated, 0))
                    AS outstanding,
                FLOOR(EXTRACT(EPOCH FROM (NOW() - s.completed_at)) / 86400)::bigint AS age_days
            FROM charges ch
            JOIN sales s ON s.id = ch.sale_id
            LEFT JOIN cancelled cx
                   ON cx.customer_id = ch.customer_id AND cx.sale_id = ch.sale_id
            LEFT JOIN sale_allocated sa
                   ON sa.customer_id = ch.customer_id AND sa.sale_id = ch.sale_id
            WHERE ch.charged - COALESCE(cx.cancelled, 0) - COALESCE(sa.allocated, 0) > 0
        ),
        aging_by_customer AS (
            SELECT
                customer_id,
                COALESCE(SUM(outstanding), 0) AS outstanding_total,
                COALESCE(SUM(CASE WHEN age_days <= 30 THEN outstanding ELSE 0 END), 0)
                    AS days_0_30,
                COALESCE(SUM(CASE WHEN age_days BETWEEN 31 AND 60 THEN outstanding ELSE 0 END), 0)
                    AS days_31_60,
                COALESCE(SUM(CASE WHEN age_days BETWEEN 61 AND 90 THEN outstanding ELSE 0 END), 0)
                    AS days_61_90,
                COALESCE(SUM(CASE WHEN age_days > 90 THEN outstanding ELSE 0 END), 0)
                    AS days_90_plus
            FROM open_sales
            GROUP BY customer_id
        )
        SELECT
            c.id AS "customer_id!",
            c.name AS "customer_name!",
            c.tax_id,
            c.is_active AS "is_active!",
            et.balance::numeric(14, 2) AS "balance!: Decimal",
            (et.received - et.refunded - COALESCE(ca.total, 0))::numeric(14, 2)
                AS "unapplied_credit!: Decimal",
            COALESCE(ag.outstanding_total, 0)::numeric(14, 2) AS "outstanding_total!: Decimal",
            et.adjustments::numeric(14, 2) AS "adjustments!: Decimal",
            COALESCE(ag.days_0_30, 0)::numeric(14, 2) AS "days_0_30!: Decimal",
            COALESCE(ag.days_31_60, 0)::numeric(14, 2) AS "days_31_60!: Decimal",
            COALESCE(ag.days_61_90, 0)::numeric(14, 2) AS "days_61_90!: Decimal",
            COALESCE(ag.days_90_plus, 0)::numeric(14, 2) AS "days_90_plus!: Decimal",
            COUNT(*) OVER () AS "items_total!",
            (SUM(et.balance) OVER ())::numeric AS "total_balance!: Decimal",
            (SUM(et.received - et.refunded - COALESCE(ca.total, 0)) OVER ())
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
        JOIN customers c ON c.id = et.customer_id
        LEFT JOIN customer_allocated ca ON ca.customer_id = et.customer_id
        LEFT JOIN aging_by_customer ag ON ag.customer_id = et.customer_id
        WHERE et.balance <> 0
           OR (et.received - et.refunded - COALESCE(ca.total, 0)) <> 0
        ORDER BY et.balance DESC, lower(c.name) ASC, c.id ASC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset
    )
    .fetch_all(&mut *conn)
    .await?;

    let Some(first) = rows.first() else {
        return Ok((Vec::new(), ReceivablesTotals::default()));
    };

    let totals = ReceivablesTotals {
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
        .map(|row| ReceivableCustomer {
            customer_id: row.customer_id,
            customer_name: row.customer_name,
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

/// What one sale still owes, on the same definition [`outstanding_by_sale_in`]
/// lists: its charge, minus the notes that cancelled part of it, minus the
/// receipts allocated to it. Derived per sale, because a writer has to check
/// one allocation without reading the customer's whole debt list.
pub async fn outstanding_for_sale_in(
    conn: &mut PgConnection,
    sale_id: Uuid,
) -> Result<Decimal, RepoError> {
    let outstanding = sqlx::query_scalar!(
        r#"
        WITH charge AS (
            SELECT COALESCE(SUM(amount), 0) AS charged
            FROM customer_account_entries
            WHERE sale_id = $1 AND kind = 'sale_charge'
        ),
        cancelled AS (
            SELECT COALESCE(-SUM(amount), 0) AS cancelled
            FROM customer_account_entries
            WHERE sale_id = $1 AND kind = 'credit_note'
        ),
        allocated AS (
            SELECT COALESCE(SUM(amount), 0) AS allocated
            FROM customer_account_allocations
            WHERE sale_id = $1
        )
        SELECT (charge.charged - cancelled.cancelled - allocated.allocated)::numeric(14, 2)
               AS "outstanding!: Decimal"
        FROM charge, cancelled, allocated
        "#,
        sale_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(outstanding)
}

/// The receipt an idempotency key already collected, or none. One small read,
/// meant to run under the customer lock and before anything is written: the
/// lock is what makes the lookup race-free, and answering a replay without
/// writing is the whole point of the key. The kind is pinned to `receipt`
/// because only a receipt carries a key today, and an idempotency key found
/// on any other kind would be a bug worth refusing rather than replaying.
pub async fn receipt_by_key_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<ExistingReceipt>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT e.id                                AS "entry_id!: Uuid",
               e.wallet_movement_id               AS "wallet_movement_id!: Uuid",
               COALESCE(
                   ARRAY_REMOVE(ARRAY_AGG(a.sale_id ORDER BY a.created_at, a.sale_id), NULL),
                   '{}'
               )                                  AS "sale_ids!: Vec<Uuid>"
        FROM customer_account_entries e
        LEFT JOIN customer_account_allocations a ON a.entry_id = e.id
        WHERE e.customer_id = $1 AND e.idempotency_key = $2 AND e.kind = 'receipt'
        GROUP BY e.id, e.wallet_movement_id
        "#,
        customer_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| ExistingReceipt {
        entry_id: row.entry_id,
        wallet_movement_id: row.wallet_movement_id,
        sale_ids: row.sale_ids,
    }))
}

/// Look up an existing credit refund by `(customer_id, idempotency_key)` under
/// the customer lock before writing anything. Pinned to `credit_refund` so a
/// key already used by a `receipt` is refused by the unique index rather than
/// replayed across kinds.
pub async fn credit_refund_by_key_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<ExistingCreditRefund>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id                 AS "entry_id!: Uuid",
               wallet_movement_id AS "wallet_movement_id!: Uuid"
        FROM customer_account_entries
        WHERE customer_id = $1 AND idempotency_key = $2 AND kind = 'credit_refund'
        "#,
        customer_id,
        idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| ExistingCreditRefund {
        entry_id: row.entry_id,
        wallet_movement_id: row.wallet_movement_id,
    }))
}
