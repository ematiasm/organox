//! Persistencia de ventas.
//!
//! El borrador y sus ítems se escriben en la misma transacción que valida el
//! stock, igual que en el libro de stock: leer y escribir separados permite
//! vender dos veces la misma unidad.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Sale, SaleItem, SaleStatus};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("no se encontró la venta")]
    NotFound,

    #[error("no se encontró el producto")]
    ProductNotFound,

    #[error("el producto no vende stock (es un servicio)")]
    ProductDoesNotTrackStock,

    #[error("stock insuficiente")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("la venta ya no es editable")]
    NotEditable,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewSaleDraft {
    pub created_by: Uuid,
}

/// Crea un borrador vacío.
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

/// Recalcula el total del borrador a partir de sus ítems.
///
/// Se recalcula siempre en el servidor: el total del cliente es solo para
/// mostrar. Devolver un total calculado por el navegador sería confiar en el
/// navegador para cobrar.
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

/// Agrega un producto al borrador.
///
/// Si el producto ya está, SUMA la cantidad. La unicidad la garantiza el índice
/// `(sale_id, product_id)` con `ON CONFLICT`, no una lectura previa: dos requests
/// simultáneas del mismo cajero (doble clic en "agregar") no deben crear dos
/// filas.
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

    // El precio se toma del producto AHORA y queda congelado en el ítem.
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

    // Un producto sin precio no se puede cobrar. Se rechaza acá en vez de
    // asumir 0, que es lo que terminaría cobrándosele al cliente.
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

/// Fija la cantidad de un ítem. Cantidad 0 lo quita.
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

/// Cancela un borrador. No toca el libro de stock porque un borrador nunca lo
/// tocó: esa es la virtud de no descontar hasta cobrar.
pub async fn cancel(pool: &PgPool, sale_id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!(
        "UPDATE sales SET status = 'cancelled' WHERE id = $1 AND status = 'draft'",
        sale_id
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // O no existe, o ya no era borrador.
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

/// El borrador abierto de un usuario, si tiene uno.
///
/// Un cajero tiene **un** borrador a la vez: abrir otro dejaría dos carritos
/// vivos y el stock reservado dos veces en la cabeza del que cobra.
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

/// Confirma la venta: valida stock, escribe los movimientos y cierra.
///
/// Todo en UNA transacción con `SELECT ... FOR UPDATE` por producto. Es la misma
/// disciplina que `record_movement`: si no, dos cajas pueden vender la última
/// unidad al mismo tiempo.
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
        // Bloquea la fila del producto: serializa las salidas concurrentes de
        // este producto, igual que en el libro de stock.
        // `query_scalar!` porque `query!` con un SELECT que devuelve filas
        // requiere `fetch_*`, no `execute`.
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

        // El movimiento de stock es la ÚNICA cosa que descuenta: la tabla
        // `sales` no tiene columna de stock, y el libro sigue siendo la verdad.
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
            Some(format!("Venta {}", &sale_id.to_string()[..8])),
            actor,
        )
        .execute(&mut *tx)
        .await?;

        movement_ids.push(movement_id);
        total += item.quantity * item.unit_price;
    }

    // `numeric(14,2)`: se redondea al guardar, no se trunca.
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

/// Ventas completadas, más recientes primero.
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
