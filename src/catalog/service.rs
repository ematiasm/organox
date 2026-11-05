//! Casos de uso del catálogo.
//!
//! Reglas de negocio que viven acá y en ningún otro lado:
//!  * un servicio no lleva movimientos de stock;
//!  * una salida no puede dejar stock negativo;
//!  * un producto con movimientos no se borra, se desactiva;
//!  * la verificación de stock y el alta del movimiento ocurren en la misma
//!    transacción.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{MovementReason, ProductKind, ProductWithStock};
use super::repository::{self, MovementRow, NewMovement, NewProduct, ProductUpdate, RepoError};
use super::validation::{self, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("el SKU ya está en uso")]
    SkuTaken,

    #[error("no se encontró el producto")]
    NotFound,

    #[error("stock insuficiente: disponible {}, solicitado {}", normalize_quantity(*available), normalize_quantity(*requested))]
    InsufficientStock {
        available: Decimal,
        requested: Decimal,
    },

    #[error("error interno del catálogo")]
    Internal(#[from] RepoError),
}

pub struct CreateProductInput {
    pub kind: ProductKind,
    pub sku: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub price: Option<Decimal>,
    pub min_stock: Option<Decimal>,
}

pub async fn create_product(
    pool: &PgPool,
    input: CreateProductInput,
) -> Result<Uuid, CatalogError> {
    let name = validation::validate_name(&input.name)?;
    let sku = validation::validate_sku(input.sku.as_deref())?;
    let price = validation::validate_price(input.price)?;
    let min_stock = validation::validate_min_stock(input.min_stock)?;

    // Un servicio no puede tener stock mínimo: no tiene stock. La DB lo impone
    // con un CHECK, así que rechazarlo acá es para dar un mensaje útil, no para
    // que el motor sea la única defensa.
    if input.kind == ProductKind::Service && min_stock.is_some() {
        return Err(ValidationError::ServiceHasNoStock.into());
    }

    let id = repository::create_product(
        pool,
        NewProduct {
            kind: input.kind,
            sku: sku.as_deref(),
            name: &name,
            description: input.description.as_deref(),
            price,
            min_stock,
        },
    )
    .await
    .map_err(map_repo_error)?;

    Ok(id)
}

pub async fn list_products(
    pool: &PgPool,
    kind: Option<ProductKind>,
    include_inactive: bool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ProductWithStock>, CatalogError> {
    // Techo defensivo: un `limit` de 10 millones es un DoS por consulta.
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);

    let search = search.map(str::trim).filter(|s| !s.is_empty());

    repository::list_products(pool, kind, include_inactive, search, limit, offset)
        .await
        .map_err(map_repo_error)
}

pub async fn get_product(pool: &PgPool, id: Uuid) -> Result<ProductWithStock, CatalogError> {
    repository::find_product(pool, id)
        .await
        .map_err(map_repo_error)?
        .ok_or(CatalogError::NotFound)
}

pub struct UpdateProductInput {
    pub sku: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub price: Option<Decimal>,
    pub min_stock: Option<Decimal>,
    pub is_active: Option<bool>,
}

pub async fn update_product(
    pool: &PgPool,
    id: Uuid,
    input: UpdateProductInput,
) -> Result<(), CatalogError> {
    // El producto debe existir antes de validar contra su tipo.
    let existing = get_product(pool, id).await?;

    let name = input
        .name
        .as_deref()
        .map(validation::validate_name)
        .transpose()?;
    let sku = match input.sku.as_deref() {
        Some(raw) => validation::validate_sku(Some(raw))?,
        None => None,
    };
    let price = validation::validate_price(input.price)?;
    let min_stock = validation::validate_min_stock(input.min_stock)?;

    if existing.product.kind == ProductKind::Service && min_stock.is_some() {
        return Err(ValidationError::ServiceHasNoStock.into());
    }

    repository::update_product(
        pool,
        id,
        ProductUpdate {
            sku: sku.as_deref(),
            name: name.as_deref(),
            description: input.description.as_deref(),
            price,
            min_stock,
            is_active: input.is_active,
        },
    )
    .await
    .map_err(map_repo_error)
}

pub async fn deactivate_product(pool: &PgPool, id: Uuid) -> Result<(), CatalogError> {
    repository::deactivate_product(pool, id)
        .await
        .map_err(map_repo_error)
}

pub struct RecordMovementInput {
    pub product_id: Uuid,
    pub reason: MovementReason,
    pub quantity: Decimal,
    pub unit_cost: Option<Decimal>,
    pub note: Option<String>,
    pub actor_id: Uuid,
}

/// Registra un movimiento de stock.
///
/// La verificación de stock disponible y el INSERT van en la **misma
/// transacción** con `SELECT ... FOR UPDATE` sobre el producto. Sin eso, dos
/// ventas simultáneas de la última unidad pueden pasar ambas: cada una lee el
/// stock antes de que la otra confirme.
pub async fn record_movement(
    pool: &PgPool,
    input: RecordMovementInput,
) -> Result<Uuid, CatalogError> {
    let quantity = validation::validate_quantity(input.quantity)?;
    let unit_cost = validation::validate_unit_cost(input.unit_cost)?;
    let note = validation::validate_note(input.note.as_deref())?;

    let mut tx = pool.begin().await.map_err(RepoError::Database)?;

    // Bloquea la fila del producto hasta el commit: serializa las salidas
    // concurrentes de este producto. El costo es contención por producto, no
    // global; es el precio correcto a pagar por no vender stock inexistente.
    let kind = sqlx::query_scalar!(
        r#"SELECT kind AS "kind: ProductKind" FROM products WHERE id = $1 FOR UPDATE"#,
        input.product_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(RepoError::Database)?
    .ok_or(CatalogError::NotFound)?;

    if !kind.tracks_stock() {
        return Err(ValidationError::ServiceHasNoStock.into());
    }

    // Si el movimiento resta, no puede dejar el stock negativo.
    if input.reason.sign() < 0 {
        let current: Decimal = sqlx::query_scalar!(
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
            input.product_id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(RepoError::Database)?;

        if current < quantity {
            return Err(CatalogError::InsufficientStock {
                available: current,
                requested: quantity,
            });
        }
    }

    let id = repository::insert_movement(
        &mut *tx,
        NewMovement {
            product_id: input.product_id,
            reason: input.reason,
            quantity,
            unit_cost,
            note: note.as_deref(),
            created_by: Some(input.actor_id),
        },
    )
    .await
    .map_err(map_repo_error)?;

    tx.commit().await.map_err(RepoError::Database)?;

    Ok(id)
}

pub async fn list_movements(
    pool: &PgPool,
    product_id: Uuid,
    limit: i64,
) -> Result<Vec<MovementRow>, CatalogError> {
    let limit = limit.clamp(1, 200);
    repository::list_movements(pool, product_id, limit)
        .await
        .map_err(map_repo_error)
}

/// Quita los ceros que agrega `numeric(12,3)` al hacer `SUM`, para que los
/// mensajes de error muestren `12.5` y no `12.500`, igual que la API.
fn normalize_quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

fn map_repo_error(err: RepoError) -> CatalogError {
    match err {
        RepoError::NotFound => CatalogError::NotFound,
        RepoError::SkuTaken => CatalogError::SkuTaken,
        other => CatalogError::Internal(other),
    }
}
