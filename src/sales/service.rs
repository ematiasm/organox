//! Sales use cases.
//!
//! Rules that live here and nowhere else:
//!  * a cashier has only one open draft;
//!  * a product without a price or a service cannot be added;
//!  * a completed sale is immutable;
//!  * a deactivated customer cannot be attached;
//!  * part of a total can go on the customer's account, but only with an
//!    active customer on the draft and room under their credit limit;
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

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("the quantity exceeds the allowed maximum")]
    QuantityTooLarge,

    #[error("the sale total exceeds the allowed maximum")]
    TotalTooLarge,

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payment amount exceeds the allowed maximum")]
    PaymentAmountTooLarge,

    #[error("the payments do not add up to the sale total")]
    PaymentMismatch {
        total: Decimal,
        paid: Decimal,
        on_account: Decimal,
    },

    #[error("the on-account amount cannot be negative")]
    OnAccountNegative,

    #[error("the on-account amount has more than 2 decimal places")]
    OnAccountPrecision,

    #[error("the on-account amount exceeds the allowed maximum")]
    OnAccountTooLarge,

    #[error("an on-account amount needs a customer on the sale")]
    OnAccountNeedsCustomer,

    #[error("an on-account amount needs an active customer")]
    OnAccountNeedsActiveCustomer,

    #[error("the on-account amount exceeds the {available} the customer still has available")]
    OnAccountExceedsCreditLimit { available: Decimal },

    #[error("the customer account is busy, retry")]
    CustomerAccountBusy,

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
        RepoError::EmptySale => SalesError::EmptySale,
        RepoError::QuantityPrecision => SalesError::QuantityPrecision,
        RepoError::QuantityTooLarge => SalesError::QuantityTooLarge,
        RepoError::TotalTooLarge => SalesError::TotalTooLarge,
        RepoError::PaymentAmountNotPositive => SalesError::PaymentAmountNotPositive,
        RepoError::PaymentAmountPrecision => SalesError::PaymentAmountPrecision,
        RepoError::PaymentAmountTooLarge => SalesError::PaymentAmountTooLarge,
        RepoError::PaymentMismatch {
            total,
            paid,
            on_account,
        } => SalesError::PaymentMismatch {
            total,
            paid,
            on_account,
        },
        RepoError::PaymentMethodUnavailable => SalesError::PaymentMethodUnavailable,
        RepoError::OnAccountNegative => SalesError::OnAccountNegative,
        RepoError::OnAccountPrecision => SalesError::OnAccountPrecision,
        RepoError::OnAccountTooLarge => SalesError::OnAccountTooLarge,
        RepoError::OnAccountNeedsCustomer => SalesError::OnAccountNeedsCustomer,
        RepoError::OnAccountNeedsActiveCustomer => SalesError::OnAccountNeedsActiveCustomer,
        RepoError::OnAccountExceedsCreditLimit { available } => {
            SalesError::OnAccountExceedsCreditLimit { available }
        }
        RepoError::CustomerAccountBusy => SalesError::CustomerAccountBusy,
        other => SalesError::Internal(other),
    }
}

/// Open a draft, or return the one the user already had open.
///
/// Returning the existing draft is correct, not a shortcut: reloading the sale
/// screen must not create a draft for every F5, or the cashier ends up with
/// twenty empty carts and the table grows pointlessly.
///
/// Returns `(id, reused)` where `reused` is `true` if an existing open draft
/// was returned and `false` if a new draft was created.
pub async fn open_draft(pool: &PgPool, user_id: Uuid) -> Result<(Uuid, bool), SalesError> {
    if let Some(existing) = repository::find_open_draft_for_user(pool, user_id)
        .await
        .map_err(map_repo_error)?
    {
        return Ok((existing, true));
    }

    let id = repository::create_draft(
        pool,
        repository::NewSaleDraft {
            created_by: user_id,
        },
    )
    .await
    .map_err(map_repo_error)?;

    Ok((id, false))
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
    if quantity.round_dp(3) != quantity {
        return Err(SalesError::QuantityPrecision);
    }
    if quantity > crate::money::MAX_QUANTITY {
        return Err(SalesError::QuantityTooLarge);
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
    if quantity > Decimal::ZERO {
        if quantity.round_dp(3) != quantity {
            return Err(SalesError::QuantityPrecision);
        }
        if quantity > crate::money::MAX_QUANTITY {
            return Err(SalesError::QuantityTooLarge);
        }
    }

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

/// Complete the sale, collecting everything at the till.
///
/// The four-argument entry point the cash-only callers already use: its
/// `on_account` is exactly the JSON body's default, zero.
pub async fn complete(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
    payments: &[SalePaymentInput],
) -> Result<CompletedSale, SalesError> {
    complete_with_on_account(pool, sale_id, actor, payments, Decimal::ZERO).await
}

/// Complete the sale with part (or all) of its total charged to the
/// customer's account.
///
/// Reject an empty cart here rather than in SQL: charging zero makes no
/// business sense and the message matters. The on-account rules themselves —
/// the invariant, a customer, an active one, a credit limit that fits — live
/// in the completion transaction, where a rejection leaves no trace.
pub async fn complete_with_on_account(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
    payments: &[SalePaymentInput],
    on_account: Decimal,
) -> Result<CompletedSale, SalesError> {
    let sale = get(pool, sale_id).await?;
    if sale.items.is_empty() {
        return Err(SalesError::EmptySale);
    }

    repository::complete(pool, sale_id, actor, payments, on_account)
        .await
        .map_err(map_repo_error)
}

/// Completed sales, most recent first, plus the total number of completed
/// sales.
///
/// The page and the count are clamped here so no caller can ask the database
/// for an unbounded window.
pub async fn list_completed(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Sale>, i64), SalesError> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);

    let (sales, total) = repository::list_completed(pool, limit, offset)
        .await
        .map_err(map_repo_error)?;

    // A page past the end returns no rows, so `COUNT(*) OVER ()` has nothing
    // to report and the repository sends `-1`. Only re-count when `offset > 0`:
    // an empty first page already proves there are zero completed sales.
    let total = if total < 0 && offset > 0 {
        repository::count_completed(pool)
            .await
            .map_err(map_repo_error)?
    } else {
        total.max(0)
    };

    Ok((sales, total))
}
