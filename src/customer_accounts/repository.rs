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
use uuid::Uuid;

use super::model::{AccountPosition, SaleOutstanding};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
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
        )
        SELECT balance::numeric(14, 2)                       AS "balance!: Decimal",
               (received - refunded - allocated.total)::numeric(14, 2)
                                                             AS "unapplied_credit!: Decimal",
               adjustments::numeric(14, 2)                   AS "adjustments!: Decimal"
        FROM entries, allocated
        "#,
        customer_id
    )
    .fetch_one(&mut *conn)
    .await?;

    // `outstanding_total` is the sum of the same rows the account statement
    // shows, so the two numbers cannot disagree.
    let debts = outstanding_by_sale_in(&mut *conn, customer_id).await?;
    let outstanding_total = debts.iter().map(|debt| debt.outstanding).sum();

    Ok(AccountPosition {
        balance: row.balance,
        unapplied_credit: row.unapplied_credit,
        outstanding_total,
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
