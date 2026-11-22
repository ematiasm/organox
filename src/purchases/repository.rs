//! Purchases persistence.
//!
//! All draft mutations lock the purchase row (`FOR UPDATE`), and completion
//! runs in one transaction that locks the purchase, supplier, and products
//! before inserting `stock_movements`, `wallet_movements`, and freezing
//! `supplier_name` + `total`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use super::model::{Purchase, PurchaseItem, PurchasePaymentInput, PurchaseStatus};
use crate::catalog::model::ProductKind;
use crate::payments::model::WalletMovementKind;
use crate::payments::repository as payments;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("purchase not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("the product is deactivated and cannot be added to a purchase")]
    ProductInactive,

    #[error("a service does not track stock and cannot be added to a purchase")]
    ProductDoesNotTrackStock,

    #[error("the purchase is already closed and cannot be modified")]
    NotEditable,

    #[error("the purchase is empty and cannot be completed")]
    EmptyPurchase,

    #[error("a supplier is required to complete a purchase")]
    SupplierRequired,

    #[error("supplier not found")]
    SupplierNotFound,

    #[error("the supplier is deactivated and cannot be used for a purchase")]
    SupplierInactive,

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payments do not add up to the purchase total")]
    PaymentMismatch { total: Decimal, paid: Decimal },

    #[error("the payment method is unknown or inactive")]
    PaymentMethodUnavailable,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Reuse the operator's latest open draft, or create a new empty draft.
pub async fn open_or_find_draft(pool: &PgPool, actor: Uuid) -> Result<(Uuid, bool), RepoError> {
    if let Some(existing) = sqlx::query_scalar!(
        r#"
        SELECT id
        FROM purchases
        WHERE created_by = $1 AND status = 'draft'
        ORDER BY created_at DESC, id DESC
        LIMIT 1
        "#,
        actor
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok((existing, true));
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO purchases (id, status, created_by)
        VALUES ($1, 'draft', $2)
        "#,
        id,
        actor,
    )
    .execute(pool)
    .await?;

    Ok((id, false))
}

async fn refresh_total_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    purchase_id: Uuid,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        UPDATE purchases
        SET total = ROUND(COALESCE((
            SELECT SUM(quantity * unit_cost) FROM purchase_items WHERE purchase_id = $1
        ), 0), 2)
        WHERE id = $1
        "#,
        purchase_id
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Attach or clear the supplier on a draft atomically (`WHERE status = 'draft'`).
pub async fn set_supplier(
    pool: &PgPool,
    purchase_id: Uuid,
    supplier_id: Option<Uuid>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE purchases
        SET supplier_id = $2
        WHERE id = $1 AND status = 'draft'
        "#,
        purchase_id,
        supplier_id,
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        let exists = sqlx::query_scalar!("SELECT 1 FROM purchases WHERE id = $1", purchase_id)
            .fetch_optional(pool)
            .await?
            .is_some();
        return Err(if exists {
            RepoError::NotEditable
        } else {
            RepoError::NotFound
        });
    }

    Ok(())
}

/// Add a product to a draft. If already present, sums quantity and updates `unit_cost`.
pub async fn upsert_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let product = sqlx::query!(
        r#"
        SELECT kind AS "kind: ProductKind", is_active
        FROM products
        WHERE id = $1
        "#,
        product_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::ProductNotFound)?;

    if !product.is_active {
        return Err(RepoError::ProductInactive);
    }
    if !product.kind.tracks_stock() {
        return Err(RepoError::ProductDoesNotTrackStock);
    }

    let item_id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (purchase_id, product_id)
        DO UPDATE SET
            quantity  = purchase_items.quantity + EXCLUDED.quantity,
            unit_cost = EXCLUDED.unit_cost
        "#,
        item_id,
        purchase_id,
        product_id,
        quantity,
        unit_cost,
    )
    .execute(&mut *tx)
    .await?;

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Replace quantity and unit cost of an existing item in a draft.
pub async fn set_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let result = sqlx::query!(
        r#"
        UPDATE purchase_items
        SET quantity = $3, unit_cost = $4
        WHERE purchase_id = $1 AND product_id = $2
        "#,
        purchase_id,
        product_id,
        quantity,
        unit_cost,
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Remove a line from a draft.
pub async fn remove_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let result = sqlx::query!(
        "DELETE FROM purchase_items WHERE purchase_id = $1 AND product_id = $2",
        purchase_id,
        product_id,
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Cancel a draft purchase. Completed or already-cancelled purchases return `NotEditable`.
pub async fn cancel(pool: &PgPool, purchase_id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!(
        "UPDATE purchases SET status = 'cancelled' WHERE id = $1 AND status = 'draft'",
        purchase_id
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        let exists = sqlx::query_scalar!("SELECT 1 FROM purchases WHERE id = $1", purchase_id)
            .fetch_optional(pool)
            .await?
            .is_some();
        return Err(if exists {
            RepoError::NotEditable
        } else {
            RepoError::NotFound
        });
    }

    Ok(())
}

pub async fn find_purchase(
    pool: &PgPool,
    purchase_id: Uuid,
) -> Result<Option<Purchase>, RepoError> {
    let Some(row) = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.status AS "status: PurchaseStatus",
            p.total,
            p.supplier_id,
            COALESCE(p.supplier_name, s.name) AS supplier_name,
            p.note,
            p.created_by,
            p.created_at,
            p.completed_at
        FROM purchases p
        LEFT JOIN suppliers s ON s.id = p.supplier_id
        WHERE p.id = $1
        "#,
        purchase_id
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let items = sqlx::query!(
        r#"
        SELECT
            pi.id,
            pi.product_id,
            pr.name AS product_name,
            pi.quantity,
            pi.unit_cost
        FROM purchase_items pi
        JOIN products pr ON pr.id = pi.product_id
        WHERE pi.purchase_id = $1
        ORDER BY pi.created_at ASC, pi.id ASC
        "#,
        purchase_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|item| PurchaseItem {
        id: item.id,
        product_id: item.product_id,
        product_name: item.product_name,
        quantity: item.quantity,
        unit_cost: item.unit_cost,
    })
    .collect();

    Ok(Some(Purchase {
        id: row.id,
        status: row.status,
        items,
        frozen_total: if row.status == PurchaseStatus::Completed {
            row.total
        } else {
            None
        },
        supplier_id: row.supplier_id,
        supplier_name: row.supplier_name,
        note: row.note,
        created_by: row.created_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
    }))
}

pub async fn list_purchases(
    pool: &PgPool,
    status: Option<PurchaseStatus>,
    supplier_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Purchase>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.status AS "status: PurchaseStatus",
            p.total,
            p.supplier_id,
            COALESCE(p.supplier_name, s.name) AS supplier_name,
            p.note,
            p.created_by,
            p.created_at,
            p.completed_at,
            COUNT(*) OVER () AS "row_total!"
        FROM purchases p
        LEFT JOIN suppliers s ON s.id = p.supplier_id
        WHERE ($1::purchase_status IS NULL OR p.status = $1)
          AND ($2::uuid IS NULL OR p.supplier_id = $2)
        ORDER BY p.created_at DESC, p.id DESC
        LIMIT $3 OFFSET $4
        "#,
        status as Option<PurchaseStatus>,
        supplier_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = rows.first().map(|r| r.row_total).unwrap_or(-1);
    if rows.is_empty() {
        return Ok((Vec::new(), total));
    }

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let item_rows = sqlx::query!(
        r#"
        SELECT
            pi.id,
            pi.purchase_id,
            pi.product_id,
            pr.name AS product_name,
            pi.quantity,
            pi.unit_cost
        FROM purchase_items pi
        JOIN products pr ON pr.id = pi.product_id
        WHERE pi.purchase_id = ANY($1)
        ORDER BY pi.created_at ASC, pi.id ASC
        "#,
        &ids
    )
    .fetch_all(pool)
    .await?;

    let mut items_by_purchase: HashMap<Uuid, Vec<PurchaseItem>> = HashMap::new();
    for item in item_rows {
        items_by_purchase
            .entry(item.purchase_id)
            .or_default()
            .push(PurchaseItem {
                id: item.id,
                product_id: item.product_id,
                product_name: item.product_name,
                quantity: item.quantity,
                unit_cost: item.unit_cost,
            });
    }

    let items = rows
        .into_iter()
        .map(|row| Purchase {
            id: row.id,
            status: row.status,
            items: items_by_purchase.remove(&row.id).unwrap_or_default(),
            frozen_total: if row.status == PurchaseStatus::Completed {
                row.total
            } else {
                None
            },
            supplier_id: row.supplier_id,
            supplier_name: row.supplier_name,
            note: row.note,
            created_by: row.created_by,
            created_at: row.created_at,
            completed_at: row.completed_at,
        })
        .collect();

    Ok((items, total))
}

pub async fn count_purchases(
    pool: &PgPool,
    status: Option<PurchaseStatus>,
    supplier_id: Option<Uuid>,
) -> Result<i64, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM purchases p
        WHERE ($1::purchase_status IS NULL OR p.status = $1)
          AND ($2::uuid IS NULL OR p.supplier_id = $2)
        "#,
        status as Option<PurchaseStatus>,
        supplier_id,
    )
    .fetch_one(pool)
    .await?;

    Ok(row.count)
}

#[derive(Debug)]
pub struct CompletedPurchase {
    pub purchase_id: Uuid,
    pub total: Decimal,
    pub movement_ids: Vec<Uuid>,
    pub payment_movement_ids: Vec<Uuid>,
    pub completed_at: DateTime<Utc>,
}

struct ResolvedPayment {
    payment_method_id: Uuid,
    wallet_id: Uuid,
    amount: Decimal,
}

async fn resolve_payments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payments_input: &[PurchasePaymentInput],
    total: Decimal,
) -> Result<Vec<ResolvedPayment>, RepoError> {
    let mut paid = Decimal::ZERO;
    let mut resolved = Vec::with_capacity(payments_input.len());

    for payment in payments_input {
        if payment.amount <= Decimal::ZERO {
            return Err(RepoError::PaymentAmountNotPositive);
        }
        if payment.amount.round_dp(2) != payment.amount {
            return Err(RepoError::PaymentAmountPrecision);
        }
        paid += payment.amount;

        let method = payments::find_method(&mut **tx, payment.payment_method_id)
            .await
            .map_err(map_payment_error)?
            .ok_or(RepoError::PaymentMethodUnavailable)?;

        if !method.is_active || !method.wallet_is_active {
            return Err(RepoError::PaymentMethodUnavailable);
        }

        resolved.push(ResolvedPayment {
            payment_method_id: method.id,
            wallet_id: method.wallet_id,
            amount: payment.amount,
        });
    }

    if paid != total {
        return Err(RepoError::PaymentMismatch { total, paid });
    }

    Ok(resolved)
}

fn map_payment_error(err: payments::RepoError) -> RepoError {
    match err {
        payments::RepoError::Database(sql) => RepoError::Database(sql),
        _ => RepoError::PaymentMethodUnavailable,
    }
}

/// Complete a draft purchase in one atomic transaction.
pub async fn complete(
    pool: &PgPool,
    purchase_id: Uuid,
    actor: Uuid,
    payments_input: &[PurchasePaymentInput],
    note: Option<&str>,
) -> Result<CompletedPurchase, RepoError> {
    let mut tx = pool.begin().await?;

    let purchase_row = sqlx::query!(
        r#"
        SELECT status AS "status: PurchaseStatus", supplier_id
        FROM purchases
        WHERE id = $1
        FOR UPDATE
        "#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !purchase_row.status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let supplier_id = purchase_row
        .supplier_id
        .ok_or(RepoError::SupplierRequired)?;

    let supplier = sqlx::query!(
        "SELECT name, is_active FROM suppliers WHERE id = $1 FOR UPDATE",
        supplier_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::SupplierNotFound)?;

    if !supplier.is_active {
        return Err(RepoError::SupplierInactive);
    }

    let items = sqlx::query!(
        r#"
        SELECT pi.product_id, pi.quantity, pi.unit_cost
        FROM purchase_items pi
        WHERE pi.purchase_id = $1
        ORDER BY pi.product_id
        "#,
        purchase_id
    )
    .fetch_all(&mut *tx)
    .await?;

    if items.is_empty() {
        return Err(RepoError::EmptyPurchase);
    }

    // Pass 1: lock each product, confirm still active and stock-tracked, compute total.
    let mut total = Decimal::ZERO;
    for item in &items {
        let product = sqlx::query!(
            r#"
            SELECT kind AS "kind: ProductKind", is_active
            FROM products
            WHERE id = $1
            FOR UPDATE
            "#,
            item.product_id
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::ProductNotFound)?;

        if !product.is_active {
            return Err(RepoError::ProductInactive);
        }
        if !product.kind.tracks_stock() {
            return Err(RepoError::ProductDoesNotTrackStock);
        }

        total += item.quantity * item.unit_cost;
    }

    let total = total.round_dp(2);

    // Validate all payments before any write.
    let charges = resolve_payments(&mut tx, payments_input, total).await?;

    let ledger_note = format!("Purchase {}", &purchase_id.to_string()[..8]);

    // Pass 2: record stock inflows.
    let mut movement_ids = Vec::with_capacity(items.len());
    for item in &items {
        let movement_id = Uuid::now_v7();
        sqlx::query!(
            r#"
            INSERT INTO stock_movements (id, product_id, reason, quantity, unit_cost, note, created_by)
            VALUES ($1, $2, 'purchase', $3, $4, $5, $6)
            "#,
            movement_id,
            item.product_id,
            item.quantity,
            Some(item.unit_cost),
            Some(ledger_note.as_str()),
            actor,
        )
        .execute(&mut *tx)
        .await?;

        movement_ids.push(movement_id);
    }

    // Pass 3: record wallet outflows.
    let mut payment_movement_ids = Vec::with_capacity(charges.len());
    for charge in &charges {
        let movement_id = payments::insert_movement(
            &mut *tx,
            payments::NewWalletMovement {
                wallet_id: charge.wallet_id,
                kind: WalletMovementKind::PurchasePayment,
                amount: charge.amount,
                sale_id: None,
                purchase_id: Some(purchase_id),
                payment_method_id: Some(charge.payment_method_id),
                credit_note_id: None,
                customer_id: None,
                note: Some(ledger_note.as_str()),
                created_by: Some(actor),
            },
        )
        .await
        .map_err(map_payment_error)?;

        payment_movement_ids.push(movement_id);
    }

    // Close the purchase and freeze supplier_name + total in the same statement.
    let completed_at = sqlx::query_scalar!(
        r#"
        UPDATE purchases
        SET status        = 'completed',
            total         = $2,
            supplier_name = $3,
            completed_by  = $4,
            completed_at  = now(),
            note          = $5
        WHERE id = $1
        RETURNING completed_at AS "completed_at!"
        "#,
        purchase_id,
        total,
        supplier.name,
        actor,
        note,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(CompletedPurchase {
        purchase_id,
        total,
        movement_ids,
        payment_movement_ids,
        completed_at,
    })
}
