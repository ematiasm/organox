//! Sales persistence.
//!
//! The draft and its items are written in the same transaction that validates
//! stock, as in the stock ledger: separate reads and writes allow the same unit
//! to be sold twice.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Sale, SaleItem, SaleStatus};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("sale not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("the product does not sell stock (it is a service)")]
    ProductDoesNotTrackStock,

    #[error("insufficient stock")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("the sale is no longer editable")]
    NotEditable,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewSaleDraft {
    pub created_by: Uuid,
}

/// Create an empty draft.
pub async fn create_draft(pool: &PgPool, new: NewSaleDraft) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO sales (id, status, created_by)
        VALUES ($1, 'draft', $2)
        "#,
        id,
        new.created_by,
    )
    .execute(pool)
    .await?;

    Ok(id)
}

/// Recalculate the draft total from its items.
///
/// Always recalculated on the server: the client's total is only for display.
/// Returning a browser-calculated total would trust the browser to charge.
async fn refresh_total(pool: &PgPool, sale_id: Uuid) -> Result<Decimal, RepoError> {
    let total: Decimal = sqlx::query_scalar!(
        r#"
        UPDATE sales
        SET total = COALESCE((
            SELECT SUM(quantity * unit_price) FROM sale_items WHERE sale_id = $1
        ), 0)
        WHERE id = $1
        RETURNING COALESCE(total, 0) AS "total!"
        "#,
        sale_id
    )
    .fetch_one(pool)
    .await?;

    Ok(total)
}

/// Add a product to the draft.
///
/// If already present, ADD the quantity. Uniqueness is guaranteed by the
/// `(sale_id, product_id)` index with `ON CONFLICT`, not a prior read: two
/// concurrent requests from the same cashier (double-clicking "add") must not
/// create two rows.
pub async fn upsert_item(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<(), RepoError> {
    let sale_status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1"#,
        sale_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !sale_status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    // Take the product price NOW and freeze it in the item.
    let product = sqlx::query!(
        r#"
        SELECT kind AS "kind: crate::catalog::model::ProductKind", name, price
        FROM products
        WHERE id = $1 AND is_active = true
        "#,
        product_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::ProductNotFound)?;

    if !product.kind.tracks_stock() {
        return Err(RepoError::ProductDoesNotTrackStock);
    }

    // A product without a price cannot be charged. Reject it here instead of
    // assuming 0, which is what the customer would otherwise be charged.
    let unit_price = product.price.ok_or(RepoError::ProductDoesNotTrackStock)?;

    sqlx::query!(
        r#"
        INSERT INTO sale_items (id, sale_id, product_id, quantity, unit_price)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (sale_id, product_id) DO UPDATE
        SET quantity = sale_items.quantity + EXCLUDED.quantity
        "#,
        Uuid::now_v7(),
        sale_id,
        product_id,
        quantity,
        unit_price,
    )
    .execute(pool)
    .await?;

    refresh_total(pool, sale_id).await?;
    Ok(())
}

/// Set an item's quantity. Quantity 0 removes it.
pub async fn set_item_quantity(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<(), RepoError> {
    let sale_status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1"#,
        sale_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !sale_status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    if quantity <= Decimal::ZERO {
        sqlx::query!(
            "DELETE FROM sale_items WHERE sale_id = $1 AND product_id = $2",
            sale_id,
            product_id
        )
        .execute(pool)
        .await?;
    } else {
        let result = sqlx::query!(
            "UPDATE sale_items SET quantity = $3 WHERE sale_id = $1 AND product_id = $2",
            sale_id,
            product_id,
            quantity
        )
        .execute(pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::ProductNotFound);
        }
    }

    refresh_total(pool, sale_id).await?;
    Ok(())
}

pub async fn remove_item(pool: &PgPool, sale_id: Uuid, product_id: Uuid) -> Result<(), RepoError> {
    set_item_quantity(pool, sale_id, product_id, Decimal::ZERO).await
}

/// Cancel a draft. Does not touch the stock ledger because a draft never did:
/// that is the virtue of not deducting until checkout.
pub async fn cancel(pool: &PgPool, sale_id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!(
        "UPDATE sales SET status = 'cancelled' WHERE id = $1 AND status = 'draft'",
        sale_id
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // Either it does not exist, or it was no longer a draft.
        let exists = sqlx::query_scalar!("SELECT 1::bool FROM sales WHERE id = $1", sale_id)
            .fetch_optional(pool)
            .await?;
        return Err(if exists.is_some() {
            RepoError::NotEditable
        } else {
            RepoError::NotFound
        });
    }

    Ok(())
}

pub async fn find_sale(pool: &PgPool, sale_id: Uuid) -> Result<Option<Sale>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id, status AS "status: SaleStatus", created_by, created_at, completed_at
        FROM sales
        WHERE id = $1
        "#,
        sale_id
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let items = sqlx::query!(
        r#"
        SELECT si.id, si.product_id, p.name AS "product_name!", si.quantity, si.unit_price
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = $1
        ORDER BY si.created_at, si.id
        "#,
        sale_id
    )
    .fetch_all(pool)
    .await?;

    Ok(Some(Sale {
        id: row.id,
        status: row.status,
        created_by: row.created_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
        items: items
            .into_iter()
            .map(|i| SaleItem {
                id: i.id,
                product_id: i.product_id,
                product_name: i.product_name,
                quantity: i.quantity,
                unit_price: i.unit_price,
            })
            .collect(),
    }))
}

/// A user's open draft, if any.
///
/// A cashier has **one** draft at a time: opening another would leave two live
/// carts and stock reserved twice in the cashier's mind.
pub async fn find_open_draft_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<Uuid>, RepoError> {
    let id = sqlx::query_scalar!(
        r#"
        SELECT id FROM sales
        WHERE status = 'draft' AND created_by = $1
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await?;

    Ok(id)
}

#[derive(Debug)]
pub struct CompletedSale {
    pub sale_id: Uuid,
    pub total: Decimal,
    pub movement_ids: Vec<Uuid>,
    pub completed_at: DateTime<Utc>,
}

/// Complete the sale: validate stock, write movements and close it.
///
/// All in ONE transaction with `SELECT ... FOR UPDATE` per product. This is the
/// same discipline as `record_movement`: otherwise two registers can sell the
/// last unit at the same time.
pub async fn complete(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
) -> Result<CompletedSale, RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1 FOR UPDATE"#,
        sale_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let items = sqlx::query!(
        r#"
        SELECT si.product_id, si.quantity, si.unit_price, p.name
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = $1
        ORDER BY si.product_id
        "#,
        sale_id
    )
    .fetch_all(&mut *tx)
    .await?;

    if items.is_empty() {
        return Err(RepoError::NotFound);
    }

    let mut movement_ids = Vec::with_capacity(items.len());
    let mut total = Decimal::ZERO;

    for item in &items {
        // Lock the product row: serialize concurrent outflows for this product,
        // as in the stock ledger.
        // `query_scalar!` because `query!` with a SELECT returning rows
        // requires `fetch_*`, not `execute`.
        sqlx::query_scalar!(
            "SELECT id FROM products WHERE id = $1 FOR UPDATE",
            item.product_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let available: Decimal = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(SUM(
                CASE reason
                    WHEN 'purchase'   THEN quantity
                    WHEN 'adjustment' THEN quantity
                    WHEN 'sale'       THEN -quantity
                    WHEN 'loss'       THEN -quantity
                END
            ), 0) AS "stock!"
            FROM stock_movements
            WHERE product_id = $1
            "#,
            item.product_id
        )
        .fetch_one(&mut *tx)
        .await?;

        if available < item.quantity {
            return Err(RepoError::InsufficientStock {
                product_name: item.name.clone(),
                available,
                requested: item.quantity,
            });
        }

        // The stock movement is the ONLY thing that deducts: the `sales` table
        // has no stock column, and the ledger remains the source of truth.
        let movement_id = Uuid::now_v7();
        sqlx::query!(
            r#"
            INSERT INTO stock_movements (id, product_id, reason, quantity, unit_cost, note, created_by)
            VALUES ($1, $2, 'sale', $3, $4, $5, $6)
            "#,
            movement_id,
            item.product_id,
            item.quantity,
            item.unit_price,
            Some(format!("Sale {}", &sale_id.to_string()[..8])),
            actor,
        )
        .execute(&mut *tx)
        .await?;

        movement_ids.push(movement_id);
        total += item.quantity * item.unit_price;
    }

    // `numeric(14,2)`: rounded on save, not truncated.
    let total = total.round_dp(2);

    let completed_at = sqlx::query_scalar!(
        r#"
        UPDATE sales
        SET status = 'completed', total = $2, completed_by = $3, completed_at = now()
        WHERE id = $1
        RETURNING completed_at AS "completed_at!"
        "#,
        sale_id,
        total,
        actor,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(CompletedSale {
        sale_id,
        total,
        movement_ids,
        completed_at,
    })
}

/// Completed sales, most recent first.
pub async fn list_completed(pool: &PgPool, limit: i64) -> Result<Vec<Sale>, RepoError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT id FROM sales
        WHERE status = 'completed'
        ORDER BY completed_at DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await?;

    let mut sales = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(sale) = find_sale(pool, id).await? {
            sales.push(sale);
        }
    }
    Ok(sales)
}
