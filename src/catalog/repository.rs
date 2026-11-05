//! Catalog data access.
//!
//! This module's important query is `list_products`: stock is calculated with
//! an aggregate subquery, not a column. The index
//! `stock_movements (product_id, created_at DESC)` makes it cheap.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{MovementReason, Product, ProductKind, ProductWithStock};
use super::validation::ValidationError;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("product not found")]
    NotFound,

    #[error("the email is already registered")]
    SkuTaken,

    #[error("the product has movements: deactivate it instead of deleting it")]
    HasMovements,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Raw `products` row mapping 1:1 to the table.
///
/// Exists because `sqlx::query!` needs an explicit destination type and the
/// domain `Product` uses newtypes. The conversion lives in `From`.
struct ProductRow {
    id: Uuid,
    kind: ProductKind,
    sku: Option<String>,
    name: String,
    description: Option<String>,
    price: Option<Decimal>,
    min_stock: Option<Decimal>,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<ProductRow> for Product {
    fn from(row: ProductRow) -> Self {
        Product {
            id: row.id,
            kind: row.kind,
            sku: row.sku,
            name: row.name,
            description: row.description,
            price: row.price,
            min_stock: row.min_stock,
            is_active: row.is_active,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

pub struct NewProduct<'a> {
    pub kind: ProductKind,
    pub sku: Option<&'a str>,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub price: Option<Decimal>,
    pub min_stock: Option<Decimal>,
}

pub async fn create_product(pool: &PgPool, new: NewProduct<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO products (id, kind, sku, name, description, price, min_stock)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        id,
        new.kind as ProductKind,
        new.sku,
        new.name,
        new.description,
        new.price,
        new.min_stock,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if is_sku_violation(&err) {
            RepoError::SkuTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

fn is_sku_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("products_sku_unique"))
}

/// Catalog query with calculated stock.
///
/// The `LEFT JOIN LATERAL` with `SUM` runs once per product and uses the index.
/// It avoids the common mistake of `N+1` queries or adding a column that must
/// later be reconciled.
///
/// The sign is applied in SQL with `CASE`, replicating `MovementReason::sign`.
/// This is the only deliberate duplication of that rule and integration tests
/// cover it by comparing both paths.
pub async fn list_products(
    pool: &PgPool,
    kind: Option<ProductKind>,
    include_inactive: bool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ProductWithStock>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.kind AS "kind: ProductKind",
            p.sku,
            p.name,
            p.description,
            p.price,
            p.min_stock,
            p.is_active,
            p.created_at,
            p.updated_at,
            s.stock
        FROM products p
        LEFT JOIN LATERAL (
            SELECT COALESCE(SUM(
                CASE m.reason
                    WHEN 'purchase'   THEN m.quantity
                    WHEN 'adjustment' THEN m.quantity
                    WHEN 'sale'       THEN -m.quantity
                    WHEN 'loss'       THEN -m.quantity
                END
            ), 0) AS stock
            FROM stock_movements m
            WHERE m.product_id = p.id
        ) s ON true
        WHERE ($1::product_kind IS NULL OR p.kind = $1::product_kind)
          AND ($2::bool OR p.is_active)
          AND ($3::text IS NULL OR p.name ILIKE '%' || $3::text || '%')
        ORDER BY p.name, p.id
        LIMIT $4 OFFSET $5
        "#,
        kind as Option<ProductKind>,
        include_inactive,
        search,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let kind = r.kind;
            let product = Product::from(ProductRow {
                id: r.id,
                kind,
                sku: r.sku,
                name: r.name,
                description: r.description,
                price: r.price,
                min_stock: r.min_stock,
                is_active: r.is_active,
                created_at: r.created_at,
                updated_at: r.updated_at,
            });

            ProductWithStock {
                // For a service stock is `None`, not `Some(0)`: "not applicable"
                // and "zero units" are different things.
                stock: kind
                    .tracks_stock()
                    .then_some(r.stock.unwrap_or(Decimal::ZERO)),
                product,
            }
        })
        .collect())
}

pub async fn find_product(pool: &PgPool, id: Uuid) -> Result<Option<ProductWithStock>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.kind AS "kind: ProductKind",
            p.sku,
            p.name,
            p.description,
            p.price,
            p.min_stock,
            p.is_active,
            p.created_at,
            p.updated_at,
            s.stock
        FROM products p
        LEFT JOIN LATERAL (
            SELECT COALESCE(SUM(
                CASE m.reason
                    WHEN 'purchase'   THEN m.quantity
                    WHEN 'adjustment' THEN m.quantity
                    WHEN 'sale'       THEN -m.quantity
                    WHEN 'loss'       THEN -m.quantity
                END
            ), 0) AS stock
            FROM stock_movements m
            WHERE m.product_id = p.id
        ) s ON true
        WHERE p.id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| {
        let kind = r.kind;
        let product = Product::from(ProductRow {
            id: r.id,
            kind,
            sku: r.sku,
            name: r.name,
            description: r.description,
            price: r.price,
            min_stock: r.min_stock,
            is_active: r.is_active,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });

        ProductWithStock {
            stock: kind
                .tracks_stock()
                .then_some(r.stock.unwrap_or(Decimal::ZERO)),
            product,
        }
    }))
}

pub struct ProductUpdate<'a> {
    pub sku: Option<&'a str>,
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub price: Option<Decimal>,
    pub min_stock: Option<Decimal>,
    pub is_active: Option<bool>,
}

/// Update only fields that are present.
///
/// `COALESCE($n, column)` makes `NULL` mean "do not touch". It deliberately
/// does not allow setting `price` to NULL (clearing a product's price); that
/// requires an explicit marker, to be added when a real use case calls for it.
pub async fn update_product(
    pool: &PgPool,
    id: Uuid,
    update: ProductUpdate<'_>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE products
        SET sku         = COALESCE($2, sku),
            name        = COALESCE($3, name),
            description = COALESCE($4, description),
            price       = COALESCE($5, price),
            min_stock   = COALESCE($6, min_stock),
            is_active   = COALESCE($7, is_active)
        WHERE id = $1
        "#,
        id,
        update.sku,
        update.name,
        update.description,
        update.price,
        update.min_stock,
        update.is_active,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if is_sku_violation(&err) {
            RepoError::SkuTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}

/// Deactivate a product. Never delete it: it has movement history and the
/// ledger cannot be modified.
///
/// Returns 404 if nonexistent. Idempotent if already inactive.
pub async fn deactivate_product(pool: &PgPool, id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!("UPDATE products SET is_active = false WHERE id = $1", id)
        .execute(pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}

pub struct NewMovement<'a> {
    pub product_id: Uuid,
    pub reason: MovementReason,
    pub quantity: Decimal,
    pub unit_cost: Option<Decimal>,
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// Generic over the executor to participate in the service transaction: the
/// `SELECT FOR UPDATE` and `INSERT` must see the same snapshot.
pub async fn insert_movement<'e, E>(executor: E, new: NewMovement<'_>) -> Result<Uuid, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO stock_movements (id, product_id, reason, quantity, unit_cost, note, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        id,
        new.product_id,
        new.reason as MovementReason,
        new.quantity,
        new.unit_cost,
        new.note,
        new.created_by,
    )
    .execute(executor)
    .await
    .map_err(|err| {
        // FK violation = product does not exist. Translate it to avoid leaking
        // constraint names to the client.
        if matches!(&err, sqlx::Error::Database(db) if db.constraint() == Some("stock_movements_product_id_fkey"))
        {
            RepoError::NotFound
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

pub struct MovementRow {
    pub id: Uuid,
    pub reason: MovementReason,
    pub quantity: Decimal,
    pub signed_quantity: Decimal,
    pub unit_cost: Option<Decimal>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Product history, newest first.
pub async fn list_movements(
    pool: &PgPool,
    product_id: Uuid,
    limit: i64,
) -> Result<Vec<MovementRow>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT
            id,
            reason AS "reason: MovementReason",
            quantity,
            CASE reason
                WHEN 'purchase'   THEN quantity
                WHEN 'adjustment' THEN quantity
                WHEN 'sale'       THEN -quantity
                WHEN 'loss'       THEN -quantity
            END AS "signed_quantity!",
            unit_cost,
            note,
            created_by,
            created_at
        FROM stock_movements
        WHERE product_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        "#,
        product_id,
        limit
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| MovementRow {
            id: r.id,
            reason: r.reason,
            quantity: r.quantity,
            signed_quantity: r.signed_quantity,
            unit_cost: r.unit_cost,
            note: r.note,
            created_by: r.created_by,
            created_at: r.created_at,
        })
        .collect())
}

/// Calculated current stock of a product.
pub async fn current_stock(pool: &PgPool, product_id: Uuid) -> Result<Decimal, RepoError> {
    let stock = sqlx::query_scalar!(
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
        product_id
    )
    .fetch_one(pool)
    .await?;

    Ok(stock)
}

/// Ensure a `ValidationError` arrives with its domain variant.
/// Used in the service's error mapping.
impl From<ValidationError> for RepoError {
    fn from(err: ValidationError) -> Self {
        match err {
            ValidationError::ProductNotFound => RepoError::NotFound,
            _ => RepoError::Database(sqlx::Error::Protocol(err.to_string())),
        }
    }
}
