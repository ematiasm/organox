//! Credit-note SQL.
//!
//! `create` is ONE transaction: the note, its lines, the stock movements and the
//! refund movements commit together or not at all. A note whose stock came back
//! but whose money did not would be worse than no note.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{
    CreditNote, CreditNoteDetail, CreditNoteItem, CreditNoteRefund, CreditableLine, ReturnLine,
    allocate_refund,
};
use crate::catalog::model::MovementReason;
use crate::payments::model::WalletMovementKind;
use crate::payments::repository::{self as wallet, NewWalletMovement};
use crate::sales::model::SaleStatus;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("sale not found")]
    SaleNotFound,

    #[error("the sale is not completed")]
    SaleNotCreditable,

    #[error("the product {product_id} is not a line of this sale")]
    ProductNotInSale { product_id: Uuid },

    #[error("cannot return {requested} of {product_name}: only {available} left to return")]
    ExceedsReturnable {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    /// The refund split must add up to the note total to the cent. It is
    /// computed, never trusted, so a mismatch is a bug and must not reach the
    /// ledger.
    #[error("the refund does not add up to the note total: total {total}, refunded {refunded}")]
    RefundDoesNotAddUp { total: Decimal, refunded: Decimal },

    #[error(transparent)]
    Wallet(#[from] wallet::RepoError),

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewCreditNote<'a> {
    pub sale_id: Uuid,
    pub reason: &'a str,
    pub lines: &'a [ReturnLine],
    pub created_by: Uuid,
}

/// The sale's lines with what each one has already given back.
///
/// Generic over the executor so the create transaction validates against the
/// same snapshot it writes to, and the read endpoint reuses it.
pub async fn creditable_lines<'e, E>(
    executor: E,
    sale_id: Uuid,
) -> Result<Vec<CreditableLine>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query!(
        r#"
        SELECT si.product_id,
               p.name AS "product_name!",
               si.quantity,
               si.unit_price,
               COALESCE((
                   SELECT SUM(cni.quantity)
                   FROM credit_note_items cni
                   JOIN credit_notes cn ON cn.id = cni.credit_note_id
                   WHERE cn.sale_id = si.sale_id AND cni.product_id = si.product_id
               ), 0) AS "credited_quantity!: Decimal"
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = $1
        ORDER BY si.created_at, si.id
        "#,
        sale_id
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CreditableLine {
            product_id: row.product_id,
            product_name: row.product_name,
            quantity: row.quantity,
            unit_price: row.unit_price,
            credited_quantity: row.credited_quantity,
        })
        .collect())
}

/// Issue a credit note: validate against the sale, then write both ledgers.
///
/// The sale row is locked `FOR UPDATE` first. Two cashiers returning the last
/// unit at the same time would otherwise both pass the "still returnable"
/// check and both write the return; serializing on the sale makes the second
/// one see the first one's note.
///
/// The products are NOT locked: a return adds stock, so it can never leave the
/// catalog negative and needs no availability check.
pub async fn create(pool: &PgPool, new: NewCreditNote<'_>) -> Result<CreditNoteDetail, RepoError> {
    let mut tx = pool.begin().await?;

    let sale = sqlx::query!(
        r#"
        SELECT status AS "status: SaleStatus"
        FROM sales
        WHERE id = $1
        FOR UPDATE
        "#,
        new.sale_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::SaleNotFound)?;

    if sale.status != SaleStatus::Completed {
        return Err(RepoError::SaleNotCreditable);
    }

    let creditable = creditable_lines(&mut *tx, new.sale_id).await?;

    // Pass 1: validate every line and compute the total from the sale's own
    // prices. Nothing is written, so a rejected line leaves no trace.
    let mut total = Decimal::ZERO;
    let mut accepted = Vec::with_capacity(new.lines.len());

    for line in new.lines {
        let sold = creditable
            .iter()
            .find(|sold| sold.product_id == line.product_id)
            .ok_or(RepoError::ProductNotInSale {
                product_id: line.product_id,
            })?;

        let available = sold.returnable_quantity();
        if line.quantity > available {
            return Err(RepoError::ExceedsReturnable {
                product_name: sold.product_name.clone(),
                available,
                requested: line.quantity,
            });
        }

        total += line.quantity * sold.unit_price;
        accepted.push((sold, line.quantity));
    }

    // `numeric(14,2)`: rounded on save, not truncated.
    let total = total.round_dp(2);

    // Pass 2: the document.
    let note_id = Uuid::now_v7();
    let created_at: DateTime<Utc> = sqlx::query_scalar!(
        r#"
        INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING created_at AS "created_at!"
        "#,
        note_id,
        new.sale_id,
        total,
        new.reason,
        new.created_by,
    )
    .fetch_one(&mut *tx)
    .await?;

    // The ledger rows point at the document by id, so a reader can always tell
    // why the stock or the money moved.
    let label = format!("credit note {note_id}: {}", new.reason);

    let mut items = Vec::with_capacity(accepted.len());

    for (sold, quantity) in accepted {
        let item_id = Uuid::now_v7();

        sqlx::query!(
            r#"
            INSERT INTO credit_note_items (id, credit_note_id, product_id, quantity, unit_price)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            item_id,
            note_id,
            sold.product_id,
            quantity,
            sold.unit_price,
        )
        .execute(&mut *tx)
        .await?;

        // Returned goods rejoin stock. Same append-only ledger as every other
        // movement; the reason carries the sign.
        sqlx::query!(
            r#"
            INSERT INTO stock_movements (id, product_id, reason, quantity, note, created_by)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            Uuid::now_v7(),
            sold.product_id,
            MovementReason::Return as MovementReason,
            quantity,
            label,
            new.created_by,
        )
        .execute(&mut *tx)
        .await?;

        items.push(CreditNoteItem {
            id: item_id,
            product_id: sold.product_id,
            product_name: sold.product_name.clone(),
            quantity,
            unit_price: sold.unit_price,
        });
    }

    // Pass 3: the money. Refund each original payment in proportion, into the
    // wallet that received it.
    let payments = sqlx::query!(
        r#"
        SELECT m.wallet_id,
               w.name AS "wallet_name!",
               m.payment_method_id AS "payment_method_id!",
               pm.name AS "payment_method_name!",
               m.amount
        FROM wallet_movements m
        JOIN wallets w ON w.id = m.wallet_id
        JOIN payment_methods pm ON pm.id = m.payment_method_id
        WHERE m.sale_id = $1 AND m.kind = 'sale_payment'
        ORDER BY m.created_at, m.id
        "#,
        new.sale_id
    )
    .fetch_all(&mut *tx)
    .await?;

    let amounts: Vec<Decimal> = payments.iter().map(|payment| payment.amount).collect();
    let shares = allocate_refund(total, &amounts);

    let refunded: Decimal = shares.iter().copied().sum();
    if refunded != total {
        return Err(RepoError::RefundDoesNotAddUp { total, refunded });
    }

    let mut refunds = Vec::with_capacity(payments.len());

    for (payment, share) in payments.iter().zip(shares) {
        // A zero share would violate `wallet_movements.amount > 0`, and a
        // movement of nothing explains nothing.
        if share <= Decimal::ZERO {
            continue;
        }

        wallet::insert_movement(
            &mut *tx,
            NewWalletMovement {
                wallet_id: payment.wallet_id,
                kind: WalletMovementKind::SaleRefund,
                amount: share,
                sale_id: Some(new.sale_id),
                payment_method_id: Some(payment.payment_method_id),
                note: Some(&label),
                created_by: Some(new.created_by),
            },
        )
        .await?;

        refunds.push(CreditNoteRefund {
            wallet_id: payment.wallet_id,
            wallet_name: payment.wallet_name.clone(),
            payment_method_id: payment.payment_method_id,
            payment_method_name: payment.payment_method_name.clone(),
            amount: share,
        });
    }

    tx.commit().await?;

    Ok(CreditNoteDetail {
        note: CreditNote {
            id: note_id,
            sale_id: new.sale_id,
            reason: new.reason.to_string(),
            total,
            created_by: new.created_by,
            created_at,
        },
        items,
        refunds,
    })
}
