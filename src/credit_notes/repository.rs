//! Credit-note SQL.
//!
//! `create` is ONE transaction: the note, its lines, the stock movements and the
//! refund movements commit together or not at all. A note whose stock came back
//! but whose money did not would be worse than no note.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use super::model::{
    CreatedCreditNote, CreditNote, CreditNoteDetail, CreditNoteItem, CreditNoteRefund,
    CreditableLine, ReturnLine, allocate_refund,
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

    /// A completed sale always has a frozen total. A NULL here would turn
    /// `already_refunded > NULL` into a silent no-op and let money escape.
    #[error("a completed sale has no stored total")]
    SaleTotalMissing,

    #[error(
        "the returns add up to {:.2} for a sale of {:.2}: only {:.2} is left to refund",
        already_refunded + requested,
        sale_total,
        sale_total - already_refunded
    )]
    RefundExceedsSale {
        sale_total: Decimal,
        already_refunded: Decimal,
        requested: Decimal,
    },

    /// The concurrent insert won, but its note is gone. No delete path exists;
    /// reaching this is a broken invariant, not a client error.
    #[error("the note from a concurrent create is no longer available")]
    ConcurrentReplayMissing,

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
    /// Trimmed by the service. A replay with the same key returns the first
    /// note instead of writing a second one.
    pub idempotency_key: Option<&'a str>,
}

/// The constraint that rejected the row, when the error came from the database.
fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}

const IDEMPOTENCY_KEY_UNIQUE: &str = "credit_notes_sale_idempotency_key_unique";

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

/// What the return screen needs for one sale: the charged lines with what is
/// left to return, and every note with its own lines and actual refunds.
#[derive(Debug)]
pub struct SaleCreditSummary {
    pub sale_id: Uuid,
    pub sale_total: Decimal,
    pub refunded_total: Decimal,
    /// The money allowance create still enforces: what the sale charged minus
    /// what its notes already refunded. Once this reaches zero, further notes
    /// are refused even if units remain.
    pub refundable_total: Decimal,
    pub lines: Vec<CreditableLine>,
    pub notes: Vec<CreditNoteDetail>,
}

/// Read a sale's credits.
///
/// The refunds come from the ledger rows that name each note, not from a
/// freshly computed split: the money that actually left the wallets is what
/// the customer was told, even if the original payment method changed.
pub async fn for_sale(
    pool: &PgPool,
    sale_id: Uuid,
) -> Result<Option<SaleCreditSummary>, RepoError> {
    let sale = sqlx::query!(
        r#"
        SELECT total FROM sales WHERE id = $1
        "#,
        sale_id
    )
    .fetch_optional(pool)
    .await?;
    let Some(sale) = sale else { return Ok(None) };

    let lines = creditable_lines(pool, sale_id).await?;
    // A draft need not have a frozen total. Round the lines like the
    // `numeric(14,2)` column would, so both paths report cents.
    let sale_total = sale.total.unwrap_or_else(|| {
        lines
            .iter()
            .map(|line| line.quantity * line.unit_price)
            .sum::<Decimal>()
            .round_dp(2)
    });

    let notes = load_notes(pool, sale_id, None).await?;
    let refunded_total = notes.iter().map(|note| note.note.total).sum::<Decimal>();
    let refundable_total = sale_total - refunded_total;

    Ok(Some(SaleCreditSummary {
        sale_id,
        sale_total,
        refunded_total,
        refundable_total,
        lines,
        notes,
    }))
}

/// One note row, before its lines and refunds are attached.
struct StoredNote {
    id: Uuid,
    sale_id: Uuid,
    reason: String,
    total: Decimal,
    created_by: Uuid,
    created_at: DateTime<Utc>,
}

/// The note rows of a sale, optionally narrowed to one idempotency key.
///
/// Generic over the executor like `creditable_lines`: the create pre-check
/// reads through its own transaction, so a keyed create holds one pool
/// connection, never two.
async fn note_rows<'e, E>(
    executor: E,
    sale_id: Uuid,
    idempotency_key: Option<&str>,
) -> Result<Vec<StoredNote>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<StoredNote> = if let Some(key) = idempotency_key {
        sqlx::query!(
            r#"
            SELECT cn.id,
                   cn.sale_id,
                   cn.reason,
                   cn.total,
                   cn.created_by,
                   cn.created_at AS "created_at!"
            FROM credit_notes cn
            WHERE cn.sale_id = $1 AND cn.idempotency_key = $2
            ORDER BY cn.created_at, cn.id
            "#,
            sale_id,
            key
        )
        .fetch_all(executor)
        .await?
        .into_iter()
        .map(|row| StoredNote {
            id: row.id,
            sale_id: row.sale_id,
            reason: row.reason,
            total: row.total,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect()
    } else {
        sqlx::query!(
            r#"
            SELECT cn.id,
                   cn.sale_id,
                   cn.reason,
                   cn.total,
                   cn.created_by,
                   cn.created_at AS "created_at!"
            FROM credit_notes cn
            WHERE cn.sale_id = $1
            ORDER BY cn.created_at, cn.id
            "#,
            sale_id
        )
        .fetch_all(executor)
        .await?
        .into_iter()
        .map(|row| StoredNote {
            id: row.id,
            sale_id: row.sale_id,
            reason: row.reason,
            total: row.total,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect()
    };

    Ok(rows)
}

/// The lines of the given notes, grouped by note id. Rows arrive in display
/// order, so grouping only has to append them.
async fn item_rows<'e, E>(
    executor: E,
    note_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<CreditNoteItem>>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let mut items: HashMap<Uuid, Vec<CreditNoteItem>> = HashMap::new();
    if note_ids.is_empty() {
        return Ok(items);
    }

    let rows = sqlx::query!(
        r#"
        SELECT cni.id,
               cni.credit_note_id,
               cni.product_id,
               p.name AS "product_name!",
               cni.quantity,
               cni.unit_price
        FROM credit_note_items cni
        JOIN products p ON p.id = cni.product_id
        WHERE cni.credit_note_id = ANY($1)
        ORDER BY p.name, cni.id
        "#,
        note_ids
    )
    .fetch_all(executor)
    .await?;

    for row in rows {
        items
            .entry(row.credit_note_id)
            .or_default()
            .push(CreditNoteItem {
                id: row.id,
                product_id: row.product_id,
                product_name: row.product_name,
                quantity: row.quantity,
                unit_price: row.unit_price,
            });
    }

    Ok(items)
}

/// The refund movements that name the given notes, grouped by note id.
async fn refund_rows<'e, E>(
    executor: E,
    note_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<CreditNoteRefund>>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let mut refunds: HashMap<Uuid, Vec<CreditNoteRefund>> = HashMap::new();
    if note_ids.is_empty() {
        return Ok(refunds);
    }

    let rows = sqlx::query!(
        r#"
        SELECT m.credit_note_id AS "credit_note_id!",
               m.wallet_id,
               w.name AS "wallet_name!",
               m.payment_method_id AS "payment_method_id!",
               pm.name AS "payment_method_name!",
               m.amount
        FROM wallet_movements m
        JOIN wallets w ON w.id = m.wallet_id
        JOIN payment_methods pm ON pm.id = m.payment_method_id
        WHERE m.kind = 'sale_refund' AND m.credit_note_id = ANY($1)
        ORDER BY m.created_at, m.id
        "#,
        note_ids
    )
    .fetch_all(executor)
    .await?;

    for row in rows {
        refunds
            .entry(row.credit_note_id)
            .or_default()
            .push(CreditNoteRefund {
                wallet_id: row.wallet_id,
                wallet_name: row.wallet_name,
                payment_method_id: row.payment_method_id,
                payment_method_name: row.payment_method_name,
                amount: row.amount,
            });
    }

    Ok(refunds)
}

/// Attach the stored lines and refunds to one note row.
fn attach_children(
    note: StoredNote,
    items: &mut HashMap<Uuid, Vec<CreditNoteItem>>,
    refunds: &mut HashMap<Uuid, Vec<CreditNoteRefund>>,
) -> CreditNoteDetail {
    CreditNoteDetail {
        note: CreditNote {
            id: note.id,
            sale_id: note.sale_id,
            reason: note.reason,
            total: note.total,
            created_by: note.created_by,
            created_at: note.created_at,
        },
        items: items.remove(&note.id).unwrap_or_default(),
        refunds: refunds.remove(&note.id).unwrap_or_default(),
    }
}

/// Notes of a sale with their lines and actual refunds, optionally narrowed
/// to one idempotency key.
///
/// The read endpoint and a create replay go through the same queries and the
/// same assembly, so a replayed note is exactly the document a later GET
/// would return. Children are batched: one query each, whatever the number of
/// notes. The pool is enough here: a replay inside `create` reads through its
/// own transaction instead, so it never waits on a second checkout.
async fn load_notes(
    pool: &PgPool,
    sale_id: Uuid,
    idempotency_key: Option<&str>,
) -> Result<Vec<CreditNoteDetail>, RepoError> {
    let notes = note_rows(pool, sale_id, idempotency_key).await?;
    let note_ids: Vec<Uuid> = notes.iter().map(|note| note.id).collect();
    let mut items = item_rows(pool, &note_ids).await?;
    let mut refunds = refund_rows(pool, &note_ids).await?;

    Ok(notes
        .into_iter()
        .map(|note| attach_children(note, &mut items, &mut refunds))
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
pub async fn create(pool: &PgPool, new: NewCreditNote<'_>) -> Result<CreatedCreditNote, RepoError> {
    let mut tx = pool.begin().await?;

    let sale = sqlx::query!(
        r#"
        SELECT status AS "status: SaleStatus", total
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

    // A retry returns the first note, whatever the rest of the request says:
    // the caller did not get its answer, not a new return to record.
    //
    // The pre-check reads through the open transaction, not the pool: a keyed
    // create holds ONE pool connection while it owns the sale lock, so a
    // saturated pool cannot turn a retry into a 500 with the lock held.
    if let Some(key) = new.idempotency_key
        && let Some(note) = note_rows(&mut *tx, new.sale_id, Some(key))
            .await?
            .into_iter()
            .next()
    {
        let note_ids = [note.id];
        let mut items = item_rows(&mut *tx, &note_ids).await?;
        let mut refunds = refund_rows(&mut *tx, &note_ids).await?;
        tx.rollback().await?;
        return Ok(CreatedCreditNote {
            detail: attach_children(note, &mut items, &mut refunds),
            replayed: true,
        });
    }

    let sale_total = sale.total.ok_or(RepoError::SaleTotalMissing)?;
    let already_refunded = sqlx::query!(
        r#"
        SELECT COALESCE(SUM(total), 0) AS "already_refunded!: Decimal"
        FROM credit_notes WHERE sale_id = $1
        "#,
        new.sale_id
    )
    .fetch_one(&mut *tx)
    .await?
    .already_refunded;

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

    // The per-product limit is not enough: rounding many small notes up can
    // return more money than the sale charged. The sale row is locked, so no
    // other note can move this sum between the read and the commit.
    if already_refunded + total > sale_total {
        return Err(RepoError::RefundExceedsSale {
            sale_total,
            already_refunded,
            requested: total,
        });
    }

    // Pass 2: the document.
    let note_id = Uuid::now_v7();
    let insert = sqlx::query_scalar!(
        r#"
        INSERT INTO credit_notes (id, sale_id, total, reason, created_by, idempotency_key)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING created_at AS "created_at!"
        "#,
        note_id,
        new.sale_id,
        total,
        new.reason,
        new.created_by,
        new.idempotency_key,
    )
    .fetch_one(&mut *tx)
    .await;

    let created_at: DateTime<Utc> = match insert {
        Ok(created_at) => created_at,
        // The sale row is locked `FOR UPDATE` before the replay pre-check, so
        // a keyed create always serializes behind the winner and the pre-check
        // above already serves the replay: this arm only runs if that lock
        // ordering ever changes. It exists so that a future caller without
        // that sale lock cannot double-write — the unique index turns the
        // second insert into a replay of the first note instead.
        Err(err) if violated_constraint(&err) == Some(IDEMPOTENCY_KEY_UNIQUE) => {
            // Two callers with the same key both found no note. The loser's
            // transaction is aborted by the unique index, so it must roll back
            // before reading the note the winner committed.
            tx.rollback().await?;
            let detail = load_notes(pool, new.sale_id, new.idempotency_key)
                .await?
                .into_iter()
                .next()
                .ok_or(RepoError::ConcurrentReplayMissing)?;
            return Ok(CreatedCreditNote {
                detail,
                replayed: true,
            });
        }
        Err(err) => return Err(err.into()),
    };

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
                credit_note_id: Some(note_id),
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

    Ok(CreatedCreditNote {
        detail: CreditNoteDetail {
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
        },
        replayed: false,
    })
}
