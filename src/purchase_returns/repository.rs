//! Purchase-return data access and atomic transaction orchestration.
//!
//! Global lock ordering matches `purchases::complete` to guarantee deadlock
//! freedom across concurrent completions, returns, supplier payments, credit
//! refunds, adjustments, sales, and deactivations:
//!
//! 1. `purchases FOR UPDATE`
//! 2. `products FOR UPDATE` (ordered by `product_id ASC`)
//! 3. Supplier advisory lock (`hashtextextended(supplier_id::text, 1)`, 5s timeout)
//! 4. `suppliers FOR UPDATE` (when checking `is_active` for `supplier_credit`)

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use super::model::{
    CreatedPurchaseReturn, PurchaseReturn, PurchaseReturnDetail, PurchaseReturnItem,
    PurchaseReturnRefund, ReturnLine, ReturnableLine, SurplusMode, split_remaining_capacity,
};
use crate::catalog::model::MovementReason;
use crate::payments::model::WalletMovementKind;
use crate::payments::repository::{self as wallet, NewWalletMovement};
use crate::purchases::model::PurchaseStatus;
use crate::supplier_accounts::repository as supplier_accounts;

const IDEMPOTENCY_KEY_UNIQUE: &str = "purchase_returns_purchase_idempotency_key_unique";

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("purchase not found")]
    PurchaseNotFound,

    #[error("the purchase is not completed")]
    PurchaseNotReturnable,

    #[error("the supplier account is busy, retry")]
    SupplierBusy,

    #[error("cannot leave credit on account for an inactive supplier")]
    SupplierInactive,

    #[error("the product {product_id} is not a line of this purchase")]
    ProductNotInPurchase { product_id: Uuid },

    #[error("cannot return {requested} of {product_name}: only {available} left to return")]
    ExceedsReturnable {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("insufficient stock for {product_name}: available {available}, requested {requested}")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("a completed purchase has no stored total")]
    PurchaseTotalMissing,

    #[error("a completed purchase has no linked supplier")]
    PurchaseSupplierMissing,

    #[error("the purchase return total exceeds the allowed maximum")]
    TotalTooLarge,

    #[error(
        "the returns add up to {:.2} for a purchase of {:.2}: only {:.2} is left to return",
        already_returned.saturating_add(*requested),
        purchase_total,
        purchase_total.saturating_sub(*already_returned)
    )]
    ReturnExceedsPurchase {
        purchase_total: Decimal,
        already_returned: Decimal,
        requested: Decimal,
    },

    #[error(
        "cannot refund {:.2} to wallets: only {:.2} remains refundable from wallet payments on this purchase (use supplier_credit instead)",
        requested,
        available
    )]
    InsufficientWalletRefundCapacity {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the return from a concurrent create is no longer available")]
    ConcurrentReplayMissing,

    #[error(
        "the wallet refund split did not add up to the surplus: surplus {surplus}, refunded {refunded}"
    )]
    RefundDoesNotAddUp { surplus: Decimal, refunded: Decimal },

    #[error(transparent)]
    Wallet(#[from] wallet::RepoError),

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewPurchaseReturn<'a> {
    pub purchase_id: Uuid,
    pub reason: &'a str,
    pub lines: &'a [ReturnLine],
    pub surplus_mode: SurplusMode,
    pub created_by: Uuid,
    pub idempotency_key: Option<&'a str>,
}

/// Summary returned by `GET /api/v1/purchases/{id}/returns`.
#[derive(Debug)]
pub struct PurchaseReturnsSummary {
    pub purchase_id: Uuid,
    pub purchase_status: PurchaseStatus,
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    pub purchase_total: Decimal,
    pub returned_total: Decimal,
    pub returnable_total: Decimal,
    pub purchase_outstanding: Decimal,
    pub wallet_refundable_capacity: Decimal,
    pub lines: Vec<ReturnableLine>,
    pub returns: Vec<PurchaseReturnDetail>,
}

struct RemainingTender {
    wallet_id: Uuid,
    wallet_name: String,
    payment_method_id: Uuid,
    payment_method_name: String,
    remaining: Decimal,
}

struct StoredReturn {
    id: Uuid,
    purchase_id: Uuid,
    total: Decimal,
    surplus_mode: SurplusMode,
    reason: String,
    created_by: Uuid,
    created_at: DateTime<Utc>,
}

fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}

fn map_account_error(err: supplier_accounts::RepoError) -> RepoError {
    match err {
        supplier_accounts::RepoError::SupplierBusy => RepoError::SupplierBusy,
        supplier_accounts::RepoError::SupplierInactive => RepoError::SupplierInactive,
        supplier_accounts::RepoError::Database(sql) => RepoError::Database(sql),
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "the supplier account refused a purchase return this transaction had validated: {other}"
        ))),
    }
}

/// Lines of the purchase with cumulative already-returned quantities.
pub async fn returnable_lines<'e, E>(
    executor: E,
    purchase_id: Uuid,
) -> Result<Vec<ReturnableLine>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query!(
        r#"
        SELECT pi.product_id,
               p.name AS "product_name!",
               pi.quantity,
               pi.unit_cost,
               COALESCE((
                   SELECT SUM(pri.quantity)
                   FROM purchase_return_items pri
                   JOIN purchase_returns pr ON pr.id = pri.purchase_return_id
                   WHERE pr.purchase_id = pi.purchase_id AND pri.product_id = pi.product_id
               ), 0) AS "returned_quantity!: Decimal"
        FROM purchase_items pi
        JOIN products p ON p.id = pi.product_id
        WHERE pi.purchase_id = $1
        ORDER BY pi.created_at, pi.id
        "#,
        purchase_id
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ReturnableLine {
            product_id: row.product_id,
            product_name: row.product_name,
            quantity: row.quantity,
            unit_cost: row.unit_cost,
            returned_quantity: row.returned_quantity,
        })
        .collect())
}

/// Remaining refundable capacity per `(wallet_id, payment_method_id)` that
/// paid money from a wallet toward this purchase (upfront `purchase_payment`
/// plus allocated `payment` entries, minus prior `purchase_refund` movements).
async fn remaining_tenders<'e, E>(
    executor: E,
    purchase_id: Uuid,
) -> Result<Vec<RemainingTender>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query!(
        r#"
        WITH paid_legs AS (
            SELECT m.wallet_id,
                   m.payment_method_id,
                   m.amount,
                   0 AS source_rank,
                   m.created_at
            FROM wallet_movements m
            WHERE m.purchase_id = $1 AND m.kind = 'purchase_payment'
            UNION ALL
            SELECT m.wallet_id,
                   m.payment_method_id,
                   a.amount,
                   1 AS source_rank,
                   e.created_at
            FROM supplier_account_allocations a
            JOIN supplier_account_entries e ON e.id = a.entry_id
            JOIN wallet_movements m ON m.id = e.wallet_movement_id
            WHERE a.purchase_id = $1 AND e.kind = 'payment'
        ),
        paid_by_tender AS (
            SELECT wallet_id,
                   payment_method_id,
                   SUM(amount) AS paid,
                   MIN(source_rank) AS min_rank,
                   MIN(created_at) AS first_at
            FROM paid_legs
            GROUP BY wallet_id, payment_method_id
        ),
        refunded_by_tender AS (
            SELECT wallet_id,
                   payment_method_id,
                   SUM(amount) AS refunded
            FROM wallet_movements
            WHERE purchase_id = $1 AND kind = 'purchase_refund'
            GROUP BY wallet_id, payment_method_id
        )
        SELECT p.wallet_id AS "wallet_id!: Uuid",
               w.name AS "wallet_name!",
               p.payment_method_id AS "payment_method_id!: Uuid",
               pm.name AS "payment_method_name!",
               GREATEST(p.paid - COALESCE(r.refunded, 0), 0)::numeric(14, 2)
                   AS "remaining!: Decimal"
        FROM paid_by_tender p
        JOIN wallets w ON w.id = p.wallet_id
        JOIN payment_methods pm ON pm.id = p.payment_method_id
        LEFT JOIN refunded_by_tender r
               ON r.wallet_id = p.wallet_id AND r.payment_method_id = p.payment_method_id
        WHERE p.paid - COALESCE(r.refunded, 0) > 0
        ORDER BY p.min_rank, p.first_at, p.wallet_id, p.payment_method_id
        "#,
        purchase_id
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| RemainingTender {
            wallet_id: row.wallet_id,
            wallet_name: row.wallet_name,
            payment_method_id: row.payment_method_id,
            payment_method_name: row.payment_method_name,
            remaining: row.remaining,
        })
        .collect())
}

async fn return_rows<'e, E>(
    executor: E,
    purchase_id: Uuid,
    idempotency_key: Option<&str>,
) -> Result<Vec<StoredReturn>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<StoredReturn> = if let Some(key) = idempotency_key {
        sqlx::query!(
            r#"
            SELECT pr.id,
                   pr.purchase_id,
                   pr.total,
                   pr.surplus_mode AS "surplus_mode: SurplusMode",
                   pr.reason,
                   pr.created_by,
                   pr.created_at AS "created_at!"
            FROM purchase_returns pr
            WHERE pr.purchase_id = $1 AND pr.idempotency_key = $2
            ORDER BY pr.created_at, pr.id
            "#,
            purchase_id,
            key
        )
        .fetch_all(executor)
        .await?
        .into_iter()
        .map(|row| StoredReturn {
            id: row.id,
            purchase_id: row.purchase_id,
            total: row.total,
            surplus_mode: row.surplus_mode,
            reason: row.reason,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect()
    } else {
        sqlx::query!(
            r#"
            SELECT pr.id,
                   pr.purchase_id,
                   pr.total,
                   pr.surplus_mode AS "surplus_mode: SurplusMode",
                   pr.reason,
                   pr.created_by,
                   pr.created_at AS "created_at!"
            FROM purchase_returns pr
            WHERE pr.purchase_id = $1
            ORDER BY pr.created_at, pr.id
            "#,
            purchase_id
        )
        .fetch_all(executor)
        .await?
        .into_iter()
        .map(|row| StoredReturn {
            id: row.id,
            purchase_id: row.purchase_id,
            total: row.total,
            surplus_mode: row.surplus_mode,
            reason: row.reason,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect()
    };

    Ok(rows)
}

async fn item_rows<'e, E>(
    executor: E,
    return_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<PurchaseReturnItem>>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let mut items: HashMap<Uuid, Vec<PurchaseReturnItem>> = HashMap::new();
    if return_ids.is_empty() {
        return Ok(items);
    }

    let rows = sqlx::query!(
        r#"
        SELECT pri.id,
               pri.purchase_return_id,
               pri.product_id,
               p.name AS "product_name!",
               pri.quantity,
               pri.unit_cost
        FROM purchase_return_items pri
        JOIN products p ON p.id = pri.product_id
        WHERE pri.purchase_return_id = ANY($1)
        ORDER BY p.name, pri.id
        "#,
        return_ids
    )
    .fetch_all(executor)
    .await?;

    for row in rows {
        items
            .entry(row.purchase_return_id)
            .or_default()
            .push(PurchaseReturnItem {
                id: row.id,
                product_id: row.product_id,
                product_name: row.product_name,
                quantity: row.quantity,
                unit_cost: row.unit_cost,
            });
    }

    Ok(items)
}

async fn refund_rows<'e, E>(
    executor: E,
    return_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<PurchaseReturnRefund>>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let mut refunds: HashMap<Uuid, Vec<PurchaseReturnRefund>> = HashMap::new();
    if return_ids.is_empty() {
        return Ok(refunds);
    }

    let rows = sqlx::query!(
        r#"
        SELECT m.purchase_return_id AS "purchase_return_id!",
               m.wallet_id,
               w.name AS "wallet_name!",
               m.payment_method_id AS "payment_method_id!",
               pm.name AS "payment_method_name!",
               m.amount
        FROM wallet_movements m
        JOIN wallets w ON w.id = m.wallet_id
        JOIN payment_methods pm ON pm.id = m.payment_method_id
        WHERE m.kind = 'purchase_refund' AND m.purchase_return_id = ANY($1)
        ORDER BY m.created_at, m.id
        "#,
        return_ids
    )
    .fetch_all(executor)
    .await?;

    for row in rows {
        refunds
            .entry(row.purchase_return_id)
            .or_default()
            .push(PurchaseReturnRefund {
                wallet_id: row.wallet_id,
                wallet_name: row.wallet_name,
                payment_method_id: row.payment_method_id,
                payment_method_name: row.payment_method_name,
                amount: row.amount,
            });
    }

    Ok(refunds)
}

/// Debt cancelled (`purchase_return`) and supplier credit granted
/// (`purchase_return_credit`) per return id.
async fn account_effects<'e, E>(
    executor: E,
    return_ids: &[Uuid],
) -> Result<HashMap<Uuid, (Decimal, Decimal)>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let mut effects: HashMap<Uuid, (Decimal, Decimal)> = HashMap::new();
    if return_ids.is_empty() {
        return Ok(effects);
    }

    let rows = sqlx::query!(
        r#"
        SELECT purchase_return_id AS "purchase_return_id!",
               COALESCE(SUM(CASE WHEN kind = 'purchase_return' THEN -amount ELSE 0 END), 0)
                   ::numeric(14, 2) AS "debt_cancelled!: Decimal",
               COALESCE(SUM(CASE WHEN kind = 'purchase_return_credit' THEN -amount ELSE 0 END), 0)
                   ::numeric(14, 2) AS "credit_granted!: Decimal"
        FROM supplier_account_entries
        WHERE purchase_return_id = ANY($1)
        GROUP BY purchase_return_id
        "#,
        return_ids
    )
    .fetch_all(executor)
    .await?;

    for row in rows {
        effects.insert(
            row.purchase_return_id,
            (row.debt_cancelled, row.credit_granted),
        );
    }

    Ok(effects)
}

fn attach_children(
    ret: StoredReturn,
    items: &mut HashMap<Uuid, Vec<PurchaseReturnItem>>,
    refunds: &mut HashMap<Uuid, Vec<PurchaseReturnRefund>>,
    effects: &mut HashMap<Uuid, (Decimal, Decimal)>,
) -> PurchaseReturnDetail {
    let (debt_cancelled, credit_granted) = effects
        .remove(&ret.id)
        .unwrap_or((Decimal::ZERO, Decimal::ZERO));
    PurchaseReturnDetail {
        ret: PurchaseReturn {
            id: ret.id,
            purchase_id: ret.purchase_id,
            total: ret.total,
            surplus_mode: ret.surplus_mode,
            reason: ret.reason,
            created_by: ret.created_by,
            created_at: ret.created_at,
        },
        debt_cancelled,
        credit_granted,
        items: items.remove(&ret.id).unwrap_or_default(),
        refunds: refunds.remove(&ret.id).unwrap_or_default(),
    }
}

async fn load_returns_in(
    conn: &mut sqlx::PgConnection,
    purchase_id: Uuid,
    idempotency_key: Option<&str>,
) -> Result<Vec<PurchaseReturnDetail>, RepoError> {
    let returns = return_rows(&mut *conn, purchase_id, idempotency_key).await?;
    let ids: Vec<Uuid> = returns.iter().map(|r| r.id).collect();
    let mut items = item_rows(&mut *conn, &ids).await?;
    let mut refunds = refund_rows(&mut *conn, &ids).await?;
    let mut effects = account_effects(&mut *conn, &ids).await?;

    Ok(returns
        .into_iter()
        .map(|ret| attach_children(ret, &mut items, &mut refunds, &mut effects))
        .collect())
}

async fn load_returns(
    pool: &PgPool,
    purchase_id: Uuid,
    idempotency_key: Option<&str>,
) -> Result<Vec<PurchaseReturnDetail>, RepoError> {
    let returns = return_rows(pool, purchase_id, idempotency_key).await?;
    let ids: Vec<Uuid> = returns.iter().map(|r| r.id).collect();
    let mut items = item_rows(pool, &ids).await?;
    let mut refunds = refund_rows(pool, &ids).await?;
    let mut effects = account_effects(pool, &ids).await?;

    Ok(returns
        .into_iter()
        .map(|ret| attach_children(ret, &mut items, &mut refunds, &mut effects))
        .collect())
}

/// Read the purchase's returnable state and issued returns in one
/// `REPEATABLE READ, READ ONLY` snapshot.
pub async fn for_purchase(
    pool: &PgPool,
    purchase_id: Uuid,
) -> Result<Option<PurchaseReturnsSummary>, RepoError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;

    let purchase = sqlx::query!(
        r#"
        SELECT p.status AS "status: PurchaseStatus",
               p.total,
               p.supplier_id,
               COALESCE(p.supplier_name, s.name) AS supplier_name
        FROM purchases p
        LEFT JOIN suppliers s ON s.id = p.supplier_id
        WHERE p.id = $1
        "#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?;

    let Some(purchase) = purchase else {
        tx.rollback().await?;
        return Ok(None);
    };

    let lines = returnable_lines(&mut *tx, purchase_id).await?;
    let purchase_total = purchase.total.unwrap_or_else(|| {
        lines
            .iter()
            .map(|line| crate::money::line_subtotal(line.quantity, line.unit_cost))
            .sum::<Decimal>()
    });

    let returns = load_returns_in(&mut tx, purchase_id, None).await?;
    let returned_total = returns.iter().map(|r| r.ret.total).sum::<Decimal>();
    let returnable_total = (purchase_total - returned_total).max(Decimal::ZERO);

    let purchase_outstanding = supplier_accounts::outstanding_for_purchase_in(&mut tx, purchase_id)
        .await
        .map_err(map_account_error)?
        .max(Decimal::ZERO);

    let wallet_refundable_capacity = remaining_tenders(&mut *tx, purchase_id)
        .await?
        .into_iter()
        .map(|t| t.remaining)
        .sum::<Decimal>()
        .min(returnable_total);

    tx.commit().await?;

    Ok(Some(PurchaseReturnsSummary {
        purchase_id,
        purchase_status: purchase.status,
        supplier_id: purchase.supplier_id,
        supplier_name: purchase.supplier_name,
        purchase_total,
        returned_total,
        returnable_total,
        purchase_outstanding,
        wallet_refundable_capacity,
        lines,
        returns,
    }))
}

/// Issue a purchase return atomically across `purchase_returns`,
/// `purchase_return_items`, `stock_movements`, `supplier_account_entries`, and
/// `wallet_movements`.
pub async fn create(
    pool: &PgPool,
    new: NewPurchaseReturn<'_>,
) -> Result<CreatedPurchaseReturn, RepoError> {
    let mut tx = pool.begin().await?;

    // Lock 1: purchase row `FOR NO KEY UPDATE` (never `FOR UPDATE`).
    //
    // `FOR NO KEY UPDATE` still serializes concurrent `purchase_returns::create`,
    // `purchases::complete`, and `purchases::cancel` on the same purchase, while
    // remaining compatible with PostgreSQL's `FOR KEY SHARE` FK RI lock acquired
    // when a concurrent `supplier_accounts::record_payment` / `allocate` inserts
    // into `supplier_account_allocations (purchase_id)` while holding the
    // supplier advisory lock. Using `FOR UPDATE` here would AB-BA deadlock with
    // `record_payment` (purchases lock <-> supplier advisory lock).
    let purchase = sqlx::query!(
        r#"
        SELECT status AS "status: PurchaseStatus", total, supplier_id
        FROM purchases
        WHERE id = $1
        FOR NO KEY UPDATE
        "#,
        new.purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::PurchaseNotFound)?;

    if purchase.status != PurchaseStatus::Completed {
        return Err(RepoError::PurchaseNotReturnable);
    }

    // Idempotent replay pre-check on the open transaction before doing work.
    if let Some(key) = new.idempotency_key
        && let Some(ret) = return_rows(&mut *tx, new.purchase_id, Some(key))
            .await?
            .into_iter()
            .next()
    {
        let ids = [ret.id];
        let mut items = item_rows(&mut *tx, &ids).await?;
        let mut refunds = refund_rows(&mut *tx, &ids).await?;
        let mut effects = account_effects(&mut *tx, &ids).await?;
        tx.rollback().await?;
        return Ok(CreatedPurchaseReturn {
            detail: attach_children(ret, &mut items, &mut refunds, &mut effects),
            replayed: true,
        });
    }

    let purchase_total = purchase.total.ok_or(RepoError::PurchaseTotalMissing)?;
    let supplier_id = purchase
        .supplier_id
        .ok_or(RepoError::PurchaseSupplierMissing)?;

    let already_returned = sqlx::query!(
        r#"
        SELECT COALESCE(SUM(total), 0) AS "already_returned!: Decimal"
        FROM purchase_returns
        WHERE purchase_id = $1
        "#,
        new.purchase_id
    )
    .fetch_one(&mut *tx)
    .await?
    .already_returned;

    let returnable = returnable_lines(&mut *tx, new.purchase_id).await?;

    // Pass 1: validate every requested line against the purchase's own lines
    // and frozen unit costs. Nothing is written yet.
    let mut total = Decimal::ZERO;
    let mut accepted = Vec::with_capacity(new.lines.len());

    for line in new.lines {
        let purchased = returnable
            .iter()
            .find(|p| p.product_id == line.product_id)
            .ok_or(RepoError::ProductNotInPurchase {
                product_id: line.product_id,
            })?;

        let available = purchased.returnable_quantity();
        if line.quantity > available {
            return Err(RepoError::ExceedsReturnable {
                product_name: purchased.product_name.clone(),
                available,
                requested: line.quantity,
            });
        }

        let subtotal = crate::money::checked_line_subtotal(line.quantity, purchased.unit_cost)
            .ok_or(RepoError::TotalTooLarge)?;
        total =
            crate::money::checked_add_amount(total, subtotal).ok_or(RepoError::TotalTooLarge)?;
        accepted.push((purchased, line.quantity));
    }

    if already_returned
        .checked_add(total)
        .is_none_or(|sum| sum > purchase_total)
    {
        return Err(RepoError::ReturnExceedsPurchase {
            purchase_total,
            already_returned,
            requested: total,
        });
    }

    // Lock 2: lock products in deterministic `product_id ASC` order and verify
    // sufficient available stock before any write.
    accepted.sort_by_key(|(purchased, _)| purchased.product_id);

    for (purchased, quantity) in &accepted {
        sqlx::query_scalar!(
            "SELECT id FROM products WHERE id = $1 FOR UPDATE",
            purchased.product_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let available: Decimal = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(SUM(
                CASE reason
                    WHEN 'purchase'        THEN quantity
                    WHEN 'return'          THEN quantity
                    WHEN 'adjustment'      THEN quantity
                    WHEN 'sale'            THEN -quantity
                    WHEN 'purchase_return' THEN -quantity
                    WHEN 'loss'            THEN -quantity
                END
            ), 0) AS "stock!"
            FROM stock_movements
            WHERE product_id = $1
            "#,
            purchased.product_id
        )
        .fetch_one(&mut *tx)
        .await?;

        if available < *quantity {
            return Err(RepoError::InsufficientStock {
                product_name: purchased.product_name.clone(),
                available,
                requested: *quantity,
            });
        }
    }

    // Lock 3 (& optional Lock 4): supplier advisory lock (seed 1), then
    // validate financial settlement completely before writing anything.
    let mut debt_cancelled = Decimal::ZERO;
    let mut credit_granted = Decimal::ZERO;
    let mut planned_refunds: Vec<(RemainingTender, Decimal)> = Vec::new();

    if total > Decimal::ZERO {
        supplier_accounts::lock_supplier(&mut tx, supplier_id)
            .await
            .map_err(map_account_error)?;

        let outstanding = supplier_accounts::outstanding_for_purchase_in(&mut tx, new.purchase_id)
            .await
            .map_err(map_account_error)?
            .max(Decimal::ZERO);

        debt_cancelled = total.min(outstanding);
        let surplus = total - debt_cancelled;

        if surplus > Decimal::ZERO {
            match new.surplus_mode {
                SurplusMode::WalletRefund => {
                    let tenders = remaining_tenders(&mut *tx, new.purchase_id).await?;
                    let capacity = tenders
                        .iter()
                        .try_fold(Decimal::ZERO, |acc, t| acc.checked_add(t.remaining))
                        .ok_or(RepoError::TotalTooLarge)?;
                    if surplus > capacity {
                        return Err(RepoError::InsufficientWalletRefundCapacity {
                            requested: surplus,
                            available: capacity,
                        });
                    }
                    let caps: Vec<Decimal> = tenders.iter().map(|t| t.remaining).collect();
                    let shares = split_remaining_capacity(surplus, &caps);
                    let refunded = shares
                        .iter()
                        .copied()
                        .try_fold(Decimal::ZERO, Decimal::checked_add)
                        .ok_or(RepoError::TotalTooLarge)?;
                    if refunded != surplus || shares.iter().any(|s| *s < Decimal::ZERO) {
                        return Err(RepoError::RefundDoesNotAddUp { surplus, refunded });
                    }
                    for (tender, share) in tenders.into_iter().zip(shares) {
                        if share > Decimal::ZERO {
                            planned_refunds.push((tender, share));
                        }
                    }
                }
                SurplusMode::SupplierCredit => {
                    let is_active = sqlx::query_scalar!(
                        "SELECT is_active FROM suppliers WHERE id = $1 FOR UPDATE",
                        supplier_id
                    )
                    .fetch_one(&mut *tx)
                    .await?;
                    if !is_active {
                        return Err(RepoError::SupplierInactive);
                    }
                    credit_granted = surplus;
                }
            }
        }
    }

    // Pass 2: insert the return header.
    let return_id = Uuid::now_v7();
    let insert = sqlx::query_scalar!(
        r#"
        INSERT INTO purchase_returns
            (id, purchase_id, total, surplus_mode, reason, idempotency_key, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING created_at AS "created_at!"
        "#,
        return_id,
        new.purchase_id,
        total,
        new.surplus_mode as SurplusMode,
        new.reason,
        new.idempotency_key,
        new.created_by,
    )
    .fetch_one(&mut *tx)
    .await;

    let created_at: DateTime<Utc> = match insert {
        Ok(created_at) => created_at,
        Err(err) if violated_constraint(&err) == Some(IDEMPOTENCY_KEY_UNIQUE) => {
            tx.rollback().await?;
            let detail = load_returns(pool, new.purchase_id, new.idempotency_key)
                .await?
                .into_iter()
                .next()
                .ok_or(RepoError::ConcurrentReplayMissing)?;
            return Ok(CreatedPurchaseReturn {
                detail,
                replayed: true,
            });
        }
        Err(err) => return Err(err.into()),
    };

    let label = format!("purchase return {return_id}: {}", new.reason);

    // Pass 3: insert return lines and stock outflows (with frozen unit_cost).
    let mut items = Vec::with_capacity(accepted.len());
    for (purchased, quantity) in accepted {
        let item_id = Uuid::now_v7();
        sqlx::query!(
            r#"
            INSERT INTO purchase_return_items
                (id, purchase_return_id, product_id, quantity, unit_cost)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            item_id,
            return_id,
            purchased.product_id,
            quantity,
            purchased.unit_cost,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO stock_movements
                (id, product_id, reason, quantity, unit_cost, note, created_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            Uuid::now_v7(),
            purchased.product_id,
            MovementReason::PurchaseReturn as MovementReason,
            quantity,
            purchased.unit_cost,
            label,
            new.created_by,
        )
        .execute(&mut *tx)
        .await?;

        items.push(PurchaseReturnItem {
            id: item_id,
            product_id: purchased.product_id,
            product_name: purchased.product_name.clone(),
            quantity,
            unit_cost: purchased.unit_cost,
        });
    }
    items.sort_by(|a, b| a.product_name.cmp(&b.product_name).then(a.id.cmp(&b.id)));

    // Pass 4: supplier-account entries and/or wallet refunds.
    if debt_cancelled > Decimal::ZERO {
        supplier_accounts::record_purchase_return(
            &mut tx,
            supplier_id,
            new.purchase_id,
            return_id,
            debt_cancelled,
            Some(&label),
            new.created_by,
        )
        .await
        .map_err(map_account_error)?;
    }

    if credit_granted > Decimal::ZERO {
        supplier_accounts::record_purchase_return_credit(
            &mut tx,
            supplier_id,
            return_id,
            credit_granted,
            Some(&label),
            new.created_by,
        )
        .await
        .map_err(map_account_error)?;
    }

    let mut refunds = Vec::with_capacity(planned_refunds.len());
    for (tender, share) in planned_refunds {
        wallet::insert_movement(
            &mut *tx,
            NewWalletMovement {
                wallet_id: tender.wallet_id,
                kind: WalletMovementKind::PurchaseRefund,
                amount: share,
                sale_id: None,
                purchase_id: Some(new.purchase_id),
                payment_method_id: Some(tender.payment_method_id),
                credit_note_id: None,
                purchase_return_id: Some(return_id),
                customer_id: None,
                supplier_id: None,
                note: Some(&label),
                created_by: Some(new.created_by),
            },
        )
        .await?;

        refunds.push(PurchaseReturnRefund {
            wallet_id: tender.wallet_id,
            wallet_name: tender.wallet_name,
            payment_method_id: tender.payment_method_id,
            payment_method_name: tender.payment_method_name,
            amount: share,
        });
    }

    tx.commit().await?;

    Ok(CreatedPurchaseReturn {
        detail: PurchaseReturnDetail {
            ret: PurchaseReturn {
                id: return_id,
                purchase_id: new.purchase_id,
                total,
                surplus_mode: new.surplus_mode,
                reason: new.reason.to_string(),
                created_by: new.created_by,
                created_at,
            },
            debt_cancelled,
            credit_granted,
            items,
            refunds,
        },
        replayed: false,
    })
}
