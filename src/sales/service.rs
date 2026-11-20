//! Sales use cases.
//!
//! Rules that live here and nowhere else:
//!  * a cashier has only one open draft;
//!  * a product without a price or a service cannot be added;
//!  * a completed sale is immutable;
//!  * a deactivated customer cannot be attached;
//!  * the server calculates the total.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Sale, SalePaymentInput};
use super::repository::{self, CompletedSale, RepoError};
use crate::customers::service::{CustomerError, get_customer};

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

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payments do not add up to the sale total")]
    PaymentMismatch { total: Decimal, paid: Decimal },

    #[error("the payment method is unknown or inactive")]
    PaymentMethodUnavailable,

    #[error("the customer is deactivated and cannot be attached to a sale")]
    CustomerInactive,

    /// Customer lookup failures that carry no sales policy. The only one
    /// reachable through `get_customer` is `NotFound`, handled above.
    #[error("customer lookup error: {0}")]
    CustomerLookup(#[from] CustomerError),

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
        RepoError::PaymentAmountNotPositive => SalesError::PaymentAmountNotPositive,
        RepoError::PaymentAmountPrecision => SalesError::PaymentAmountPrecision,
        RepoError::PaymentMismatch { total, paid } => SalesError::PaymentMismatch { total, paid },
        RepoError::PaymentMethodUnavailable => SalesError::PaymentMethodUnavailable,
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

/// Attach or clear the sale's customer. Draft-only (the repository enforces
/// it), and only an active customer can be attached.
///
/// Existence and active state reuse the customers module's own `get_customer`:
/// the rule "who is attachable" already lives there, and a second copy in
/// sales would drift out of sync with it.
pub async fn set_customer(
    pool: &PgPool,
    sale_id: Uuid,
    customer_id: Option<Uuid>,
) -> Result<(), SalesError> {
    if let Some(id) = customer_id {
        let customer = get_customer(pool, id).await.map_err(|err| match err {
            CustomerError::NotFound => SalesError::NotFound,
            other => SalesError::CustomerLookup(other),
        })?;

        if !customer.is_active {
            return Err(SalesError::CustomerInactive);
        }
    }

    repository::set_customer(pool, sale_id, customer_id)
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
    payments: &[SalePaymentInput],
) -> Result<CompletedSale, SalesError> {
    let sale = get(pool, sale_id).await?;
    if sale.items.is_empty() {
        return Err(SalesError::EmptySale);
    }

    repository::complete(pool, sale_id, actor, payments)
        .await
        .map_err(map_repo_error)
}

pub async fn list_completed(pool: &PgPool, limit: i64) -> Result<Vec<Sale>, SalesError> {
    repository::list_completed(pool, limit.clamp(1, 200))
        .await
        .map_err(map_repo_error)
}
