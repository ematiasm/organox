//! Purchases use cases.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Purchase, PurchasePaymentInput, PurchaseStatus};
use super::repository::{self, CompletedPurchase, RepoError};
use crate::suppliers::service::{SupplierError, get_supplier};

/// Maximum `numeric(12, 3)` quantity accepted at the boundary.
fn max_quantity() -> Decimal {
    Decimal::new(999_999_999_999, 3)
}

/// Maximum `numeric(12, 2)` unit cost accepted at the boundary.
fn max_unit_cost() -> Decimal {
    Decimal::new(999_999_999_999, 2)
}

#[derive(Debug, thiserror::Error)]
pub enum PurchaseError {
    #[error("purchase not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("a service does not track stock and cannot be added to a purchase")]
    ServiceNotPurchasable,

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

    #[error("the quantity must be greater than zero")]
    QuantityNotPositive,

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("the quantity exceeds the allowed maximum")]
    QuantityTooLarge,

    #[error("the unit cost cannot be negative")]
    UnitCostNegative,

    #[error("the unit cost has more than 2 decimal places")]
    UnitCostPrecision,

    #[error("the unit cost exceeds the allowed maximum")]
    UnitCostTooLarge,

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payments ({paid:.2}) do not add up to the purchase total ({total:.2})")]
    PaymentMismatch { total: Decimal, paid: Decimal },

    #[error("the payment method is unknown or inactive")]
    PaymentMethodUnavailable,

    #[error("supplier lookup error: {0}")]
    SupplierLookup(#[from] SupplierError),

    #[error("internal purchase error")]
    Internal(#[from] RepoError),
}

fn validate_line(quantity: Decimal, unit_cost: Decimal) -> Result<(), PurchaseError> {
    if quantity <= Decimal::ZERO {
        return Err(PurchaseError::QuantityNotPositive);
    }
    if quantity.round_dp(3) != quantity {
        return Err(PurchaseError::QuantityPrecision);
    }
    if quantity > max_quantity() {
        return Err(PurchaseError::QuantityTooLarge);
    }
    if unit_cost < Decimal::ZERO {
        return Err(PurchaseError::UnitCostNegative);
    }
    if unit_cost.round_dp(2) != unit_cost {
        return Err(PurchaseError::UnitCostPrecision);
    }
    if unit_cost > max_unit_cost() {
        return Err(PurchaseError::UnitCostTooLarge);
    }
    Ok(())
}

pub async fn open_draft(pool: &PgPool, actor: Uuid) -> Result<(Uuid, bool), PurchaseError> {
    repository::open_or_find_draft(pool, actor)
        .await
        .map_err(map_repo_error)
}

pub async fn get_purchase(pool: &PgPool, id: Uuid) -> Result<Purchase, PurchaseError> {
    repository::find_purchase(pool, id)
        .await
        .map_err(map_repo_error)?
        .ok_or(PurchaseError::NotFound)
}

pub async fn list_purchases(
    pool: &PgPool,
    status: Option<PurchaseStatus>,
    supplier_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Purchase>, i64), PurchaseError> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);

    let (items, total) = repository::list_purchases(pool, status, supplier_id, limit, offset)
        .await
        .map_err(map_repo_error)?;

    let total = if total < 0 {
        repository::count_purchases(pool, status, supplier_id)
            .await
            .map_err(map_repo_error)?
    } else {
        total
    };

    Ok((items, total))
}

pub async fn set_supplier(
    pool: &PgPool,
    purchase_id: Uuid,
    supplier_id: Option<Uuid>,
) -> Result<(), PurchaseError> {
    if let Some(id) = supplier_id {
        let supplier = match get_supplier(pool, id).await {
            Ok(supplier) => supplier,
            Err(SupplierError::NotFound) => return Err(PurchaseError::SupplierNotFound),
            Err(other) => return Err(PurchaseError::SupplierLookup(other)),
        };
        if !supplier.is_active {
            return Err(PurchaseError::SupplierInactive);
        }
    }

    repository::set_supplier(pool, purchase_id, supplier_id)
        .await
        .map_err(map_repo_error)
}

pub async fn add_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<Purchase, PurchaseError> {
    validate_line(quantity, unit_cost)?;
    repository::upsert_item(pool, purchase_id, product_id, quantity, unit_cost)
        .await
        .map_err(map_repo_error)?;
    get_purchase(pool, purchase_id).await
}

pub async fn set_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<Purchase, PurchaseError> {
    validate_line(quantity, unit_cost)?;
    repository::set_item(pool, purchase_id, product_id, quantity, unit_cost)
        .await
        .map_err(map_repo_error)?;
    get_purchase(pool, purchase_id).await
}

pub async fn remove_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
) -> Result<Purchase, PurchaseError> {
    repository::remove_item(pool, purchase_id, product_id)
        .await
        .map_err(map_repo_error)?;
    get_purchase(pool, purchase_id).await
}

pub async fn complete(
    pool: &PgPool,
    purchase_id: Uuid,
    actor: Uuid,
    payments: &[PurchasePaymentInput],
    note: Option<&str>,
) -> Result<CompletedPurchase, PurchaseError> {
    let clean_note = note.map(str::trim).filter(|s| !s.is_empty());
    repository::complete(pool, purchase_id, actor, payments, clean_note)
        .await
        .map_err(map_repo_error)
}

pub async fn cancel(pool: &PgPool, purchase_id: Uuid) -> Result<(), PurchaseError> {
    repository::cancel(pool, purchase_id)
        .await
        .map_err(map_repo_error)
}

fn map_repo_error(err: RepoError) -> PurchaseError {
    match err {
        RepoError::NotFound => PurchaseError::NotFound,
        RepoError::ProductNotFound => PurchaseError::ProductNotFound,
        RepoError::ProductDoesNotTrackStock => PurchaseError::ServiceNotPurchasable,
        RepoError::NotEditable => PurchaseError::NotEditable,
        RepoError::EmptyPurchase => PurchaseError::EmptyPurchase,
        RepoError::SupplierRequired => PurchaseError::SupplierRequired,
        RepoError::SupplierNotFound => PurchaseError::SupplierNotFound,
        RepoError::SupplierInactive => PurchaseError::SupplierInactive,
        RepoError::PaymentAmountNotPositive => PurchaseError::PaymentAmountNotPositive,
        RepoError::PaymentAmountPrecision => PurchaseError::PaymentAmountPrecision,
        RepoError::PaymentMismatch { total, paid } => {
            PurchaseError::PaymentMismatch { total, paid }
        }
        RepoError::PaymentMethodUnavailable => PurchaseError::PaymentMethodUnavailable,
        other => PurchaseError::Internal(other),
    }
}
