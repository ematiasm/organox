//! Casos de uso de ventas.
//!
//! Reglas que viven acá y en ningún otro lado:
//!  * un cajero tiene un solo borrador abierto;
//!  * no se agrega un producto sin precio ni un servicio;
//!  * una venta completada es inmutable;
//!  * el total lo calcula el servidor.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::Sale;
use super::repository::{self, CompletedSale, RepoError};

#[derive(Debug, thiserror::Error)]
pub enum SalesError {
    #[error("no se encontró la venta")]
    NotFound,

    #[error("no se encontró el producto")]
    ProductNotFound,

    #[error("un servicio no lleva stock: no se puede vender como ítem de una venta")]
    ServiceNotSellable,

    #[error("stock insuficiente de {product_name}: disponible {available}, pedido {requested}")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("la venta ya fue cerrada y no se puede modificar")]
    NotEditable,

    #[error("la venta está vacía: no se puede cobrar")]
    EmptySale,

    #[error("la cantidad debe ser mayor que cero")]
    QuantityNotPositive,

    #[error("error interno de ventas")]
    Internal(#[from] RepoError),
}

fn map_repo_error(err: RepoError) -> SalesError {
    match err {
        RepoError::NotFound => SalesError::NotFound,
        RepoError::ProductNotFound => SalesError::ProductNotFound,
        RepoError::ProductDoesNotTrackStock => SalesError::ServiceNotSellable,
        RepoError::InsufficientStock {
            product_name,
            available,
            requested,
        } => SalesError::InsufficientStock {
            product_name,
            available,
            requested,
        },
        RepoError::NotEditable => SalesError::NotEditable,
        other => SalesError::Internal(other),
    }
}

/// Abre un borrador, o devuelve el que el usuario ya tenía abierto.
///
/// Devolver el existente es lo correcto y no un atajo: recargar la pantalla de
/// venta no debe crear un borrador nuevo por cada F5, o el cajero termina con
/// veinte carritos vacíos y la tabla crece sin sentido.
pub async fn open_draft(pool: &PgPool, user_id: Uuid) -> Result<Uuid, SalesError> {
    if let Some(existing) = repository::find_open_draft_for_user(pool, user_id)
        .await
        .map_err(map_repo_error)?
    {
        return Ok(existing);
    }

    repository::create_draft(
        pool,
        repository::NewSaleDraft {
            created_by: user_id,
        },
    )
    .await
    .map_err(map_repo_error)
}

pub async fn get(pool: &PgPool, sale_id: Uuid) -> Result<Sale, SalesError> {
    repository::find_sale(pool, sale_id)
        .await
        .map_err(map_repo_error)?
        .ok_or(SalesError::NotFound)
}

pub async fn add_item(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<Sale, SalesError> {
    if quantity <= Decimal::ZERO {
        return Err(SalesError::QuantityNotPositive);
    }

    repository::upsert_item(pool, sale_id, product_id, quantity)
        .await
        .map_err(map_repo_error)?;

    get(pool, sale_id).await
}

pub async fn set_item_quantity(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<Sale, SalesError> {
    repository::set_item_quantity(pool, sale_id, product_id, quantity)
        .await
        .map_err(map_repo_error)?;

    get(pool, sale_id).await
}

pub async fn remove_item(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
) -> Result<Sale, SalesError> {
    repository::remove_item(pool, sale_id, product_id)
        .await
        .map_err(map_repo_error)?;

    get(pool, sale_id).await
}

pub async fn cancel(pool: &PgPool, sale_id: Uuid) -> Result<(), SalesError> {
    repository::cancel(pool, sale_id)
        .await
        .map_err(map_repo_error)
}

/// Cobra la venta.
///
/// Rechaza un carrito vacío acá y no en SQL: cobrar cero no tiene sentido de
/// negocio y el mensaje importa.
pub async fn complete(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
) -> Result<CompletedSale, SalesError> {
    let sale = get(pool, sale_id).await?;
    if sale.items.is_empty() {
        return Err(SalesError::EmptySale);
    }

    repository::complete(pool, sale_id, actor)
        .await
        .map_err(map_repo_error)
}

pub async fn list_completed(pool: &PgPool, limit: i64) -> Result<Vec<Sale>, SalesError> {
    repository::list_completed(pool, limit.clamp(1, 200))
        .await
        .map_err(map_repo_error)
}
