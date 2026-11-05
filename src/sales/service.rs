//! Sales use cases.
//!
//! Rules that live here and nowhere else:
//!  * a cashier has only one open draft;
//!  * a product without a price or a service cannot be added;
//!  * a completed sale is immutable;
//!  * the server calculates the total.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::Sale;
use super::repository::{self, CompletedSale, RepoError};

#[derive(Debug, thiserror::Error)]
pub enum SalesError {
    #[error("sale not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("a service does not track stock: it cannot be sold as a sale item")]
    ServiceNotSellable,

    #[error("insufficient stock for {product_name}: available {available}, requested {requested}")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("the sale is already closed and cannot be modified")]
    NotEditable,

    #[error("the sale is empty and cannot be completed")]
    EmptySale,

    #[error("the quantity must be greater than zero")]
    QuantityNotPositive,

    #[error("internal sales error")]
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

/// Open a draft, or return the one the user already had open.
///
/// Returning the existing draft is correct, not a shortcut: reloading the sale
/// screen must not create a draft for every F5, or the cashier ends up with
/// twenty empty carts and the table grows pointlessly.
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

/// Complete the sale.
///
/// Reject an empty cart here rather than in SQL: charging zero makes no
/// business sense and the message matters.
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
