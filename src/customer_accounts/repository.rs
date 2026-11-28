//! Customer accounts data access.
//!
//! Two sums carry this module: the balance is `SUM(amount)` over the entries,
//! and a sale's outstanding amount is its charge minus the notes that cancelled
//! part of it minus the receipts allocated to it. Neither is stored, so neither
//! can drift.
//!
//! `unapplied_credit` is the third: money collected, not allocated to a sale and
//! not handed back. It is what an operator can still apply, and the reason a
//! customer can owe on one sale while holding money for the next.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::model::{
    AccountPosition, AdjustmentLogItem, AgingBuckets, CustomerAccountEntry,
    CustomerAccountEntryKind, ReceiptLogAllocation, ReceiptLogItem, ReceivableCustomer,
    ReceivablesTotals, SaleOutstanding,
};
use crate::db::violated_constraint;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error("customer not found")]
    CustomerNotFound,

    #[error("sale not found")]
    SaleNotFound,

    #[error("account entry not found")]
    EntryNotFound,

    #[error("that sale belongs to another customer")]
    SaleOfAnotherCustomer,

    #[error("that sale is not completed")]
    SaleNotChargeable,

    #[error("this sale is already charged to the account")]
    AlreadyCharged,

    #[error("the amount must be positive")]
    NonPositiveAmount,

    #[error("the amount {amount} carries more than two decimals")]
    TooManyDecimals { amount: Decimal },

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("the allocation of {requested} exceeds the {outstanding} this sale still owes")]
    OutstandingExceeded {
        requested: Decimal,
        outstanding: Decimal,
    },

    #[error("the allocations ({allocated}) exceed the money the receipt collected ({collected})")]
    ReceiptOverAllocated {
        allocated: Decimal,
        collected: Decimal,
    },

    #[error("this receipt collected {collected} and {allocated} is already allocated")]
    AllocationExceedsReceipt {
        collected: Decimal,
        allocated: Decimal,
    },

    #[error("the allocation of {requested} exceeds the {available} the customer holds in credit")]
    AllocationExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("only a receipt can be allocated to a sale")]
    NotAReceipt,

    #[error("wallet movement not found")]
    MovementNotFound,

    #[error("the receipt records {receipt} but the wallet movement carries {movement}")]
    ReceiptDoesNotMatchMovement { movement: Decimal, receipt: Decimal },

    #[error("the refund of {requested} exceeds the {available} the customer holds in credit")]
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

    #[error("the charge of {requested} exceeds the {available} the sale leaves unpaid")]
    ChargeExceedsSaleTotal {
        requested: Decimal,
        available: Decimal,
    },

    #[error("credit note not found")]
    CreditNoteNotFound,

    #[error("that credit note belongs to another sale")]
    CreditNoteMismatch,

    #[error("the note cancels {requested} but the note is only {available}")]
    CreditNoteExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the same sale appears twice in one receipt")]
    DuplicateAllocation { sale_id: Uuid },

    #[error("an account entry cannot be backed by a {kind} movement")]
    MovementNotFromAnAccount { kind: String },

    #[error("the wallet movement belongs to another customer")]
    MovementOfAnotherCustomer,

    #[error("the customer account is busy, retry")]
    CustomerBusy,

    #[error("the customer is deactivated")]
    CustomerInactive,

    #[error("the account would reach {requested} and only {available} is available")]
    CreditLimitExceeded {
        requested: Decimal,
        available: Decimal,
    },

    /// The caps inside `apply_available_credit` re-derive everything the
    /// ledger's own guards check, so a guard firing there is an impossible
    /// ledger violation, not a client error. Typed so the completion can
    /// answer an internal error without pretending a database protocol
    /// failed.
    #[error("the credit application disagreed with the ledger's own guards")]
    CreditApplicationBroken,
}

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

// --- Writes ------------------------------------------------------------

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

/// A collection: the movement that carried the money, and the sales it pays.
///
/// `allocations` is what the operator decided, never something the server
/// invents; the remainder of `amount` carries no row anywhere and that is
/// exactly what an available credit is.
pub struct NewReceipt<'a> {
    pub customer_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}

/// The receipt as recorded, or as it already was: a replay answers the first
/// entry so a retried collection never collects twice.
pub struct RecordedReceipt {
    pub entry_id: Uuid,
    pub replayed: bool,
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

    // A repeated sale in one receipt is the same allocation twice. The
    // engine's `UNIQUE (entry_id, sale_id)` would refuse it, but as an
    // untyped database error and only after the entry had been written;
    // refusing it here keeps the rejection typed and the ledger untouched.
    let mut seen = HashSet::new();
    for (sale_id, _) in receipt.allocations {
        if !seen.insert(*sale_id) {
            return Err(RepoError::DuplicateAllocation { sale_id: *sale_id });
        }
    }

    if let Some(key) = receipt.idempotency_key {
        let first = sqlx::query_scalar!(
            r#"
            SELECT id FROM customer_account_entries
            WHERE customer_id = $1 AND idempotency_key = $2 AND kind = 'receipt'
            "#,
            receipt.customer_id,
            key,
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(entry_id) = first {
            return Ok(RecordedReceipt {
                entry_id,
                replayed: true,
            });
        }
    }

    // Each allocation is checked against what its sale still owes before
    // anything is written; the sum is checked last so a receipt that pays one
    // sale too much names the sale, not the sum.
    let mut allocated = Decimal::ZERO;
    for (sale_id, amount) in receipt.allocations {
        ensure_cents(*amount)?;
        ensure_chargeable_sale(conn, receipt.customer_id, *sale_id).await?;

        let outstanding = outstanding_for_sale_in(conn, *sale_id).await?;
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
    if allocated > receipt.amount {
        return Err(RepoError::ReceiptOverAllocated {
            allocated,
            collected: receipt.amount,
        });
    }

    let entry_id = Uuid::now_v7();
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

    for (sale_id, amount) in receipt.allocations {
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

    Ok(RecordedReceipt {
        entry_id,
        replayed: false,
    })
}

/// The receipt a replay must answer with: the entry as it already was, the
/// movement that carried its money, and the sales its allocations pay —
/// everything the response needs, so a replay never re-derives anything from
/// the request that retried it.
pub struct ExistingReceipt {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub sale_ids: Vec<Uuid>,
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

    // Read before locking: the entry is append-only, so this row cannot change
    // under us; only the allocation sum below needs the lock.
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

    lock_customer(conn, entry.customer_id).await?;

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

    let collected = -entry.amount;
    if already
        .checked_add(amount)
        .is_none_or(|sum| sum > collected)
    {
        return Err(RepoError::AllocationExceedsReceipt {
            collected,
            allocated: already,
        });
    }

    ensure_chargeable_sale(conn, entry.customer_id, sale_id).await?;

    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

    // Both the account-level `unapplied_credit` ceiling and this receipt's own
    // unrefunded remainder must cover the allocation: money handed back via a
    // `credit_refund` is attributed to specific receipts, so a refunded receipt
    // cannot be reused to back future sales even when newer receipts keep
    // account-level `unapplied_credit` positive.
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

    let position = account_position_in(conn, entry.customer_id).await?;
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
    // Two ceilings decide before any receipt is read: what the caller asked
    // for, and what the account can actually pay with. Per-receipt refund
    // attributions below also exclude refunded receipts so credit-note refunds
    // route through the wallet that actually holds the remaining credit.
    let position = account_position_in(conn, customer_id).await?;
    let cap = up_to.min(position.unapplied_credit);
    if cap <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    // The sale is the third ceiling: applying more than it owes would turn
    // credit the customer keeps into a phantom overpayment.
    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    let mut remaining = cap.min(outstanding);
    if remaining <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }

    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    // All receipts that still carry unallocated and unrefunded money, oldest
    // first. The `id` tiebreaker keeps the order total: two receipts created in
    // the same instant are still one before the other, and the ids are
    // time-ordered. Allocations are planned in memory from this locked snapshot
    // and inserted in a single batched query so fragmented accounts neither hit
    // a silent row cap nor pay N+1 query overhead while holding advisory locks.
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

    sqlx::query!(
        r#"
        INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
        SELECT id, entry_id, $3, amount
        FROM UNNEST($1::uuid[], $2::uuid[], $4::numeric(14, 2)[])
            AS planned(id, entry_id, amount)
        "#,
        &ids,
        &entry_ids,
        sale_id,
        &amounts,
    )
    .execute(&mut *conn)
    .await?;

    Ok(applied)
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

/// The credit refund an idempotency key already recorded, or none.
pub struct ExistingCreditRefund {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
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

/// Input for recording a credit refund with an optional idempotency key.
pub struct NewCreditRefund<'a> {
    pub customer_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
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

    // A replay answers by identity, not by re-running the numbers: the first
    // refund already consumed the credit this one claims, so affordability
    // alone could not tell a replay from a fresh refund that happens to be
    // unaffordable. The lock makes the lookup race-free; the unique index
    // stays as the backstop.
    let already = sqlx::query_scalar!(
        r#"
        SELECT id FROM customer_account_entries
        WHERE wallet_movement_id = $1
        "#,
        refund.wallet_movement_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if already.is_some() {
        return Err(RepoError::AlreadyRecorded);
    }

    let position = account_position_in(conn, refund.customer_id).await?;

    // Money is only handed back against money the shop actually holds: the
    // unapplied credit says it was collected, the negative balance says it was
    // never spent on debt. Either alone could lie — unapplied credit next to
    // a debt, or a negative balance with the credit already refunded — so the
    // refund sees the smaller of the two, floored at zero because a customer
    // who owes has nothing to claim back at all.
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

    // Attribute the refunded credit to the receipts that hold unapplied credit:
    // consume receipts from the same wallet first (preserving physical wallet
    // correspondence), then oldest-first across remaining receipts.
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
        refund.customer_id,
        refund_wallet_id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut remaining = refund.amount;
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
            id,
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
