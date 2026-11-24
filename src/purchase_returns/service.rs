//! Purchase-return use cases and input validation.

use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;

use super::model::{
    CreatedPurchaseReturn, MAX_IDEMPOTENCY_KEY_LEN, MAX_REASON_LEN, ReturnLine, SurplusMode,
};
use super::repository::{self, NewPurchaseReturn, PurchaseReturnsSummary, RepoError};

#[derive(Debug, thiserror::Error)]
pub enum PurchaseReturnsError {
    #[error("purchase not found")]
    PurchaseNotFound,

    #[error("the purchase is not completed: only a completed purchase can be returned")]
    PurchaseNotReturnable,

    #[error("a purchase return needs at least one line")]
    Empty,

    #[error("the quantity must be greater than zero")]
    QuantityNotPositive,

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("a product appears twice in the same purchase return")]
    DuplicateProduct,

    #[error("the reason is required")]
    ReasonMissing,

    #[error("the reason is longer than {MAX_REASON_LEN} characters")]
    ReasonTooLong,

    #[error("an idempotency key cannot be blank")]
    IdempotencyKeyMissing,

    #[error("the idempotency key is longer than {MAX_IDEMPOTENCY_KEY_LEN} characters")]
    IdempotencyKeyTooLong,

    #[error("the product is not a line of this purchase")]
    ProductNotInPurchase,

    #[error("cannot return {requested} of {product_name}: only {available} left to return")]
    ExceedsReturnable {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("insufficient stock for {product_name}: available {available}, requested {requested}")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error(
        "the returns add up to {:.2} for a purchase of {:.2}: only {:.2} is left to return",
        already_returned + requested,
        purchase_total,
        purchase_total - already_returned
    )]
    ReturnExceedsPurchase {
        purchase_total: Decimal,
        already_returned: Decimal,
        requested: Decimal,
    },

    #[error(
        "cannot refund {:.2} to wallets: only {:.2} remains refundable from wallet payments on this purchase (use supplier_credit instead)",
        requested,
        available
    )]
    InsufficientWalletRefundCapacity {
        requested: Decimal,
        available: Decimal,
    },

    #[error("cannot leave credit on account for an inactive supplier")]
    SupplierInactive,

    #[error("the supplier account is busy, retry")]
    SupplierBusy,

    #[error("internal purchase-return error")]
    Internal(#[from] RepoError),
}

fn map_repo_error(err: RepoError) -> PurchaseReturnsError {
    match err {
        RepoError::PurchaseNotFound => PurchaseReturnsError::PurchaseNotFound,
        RepoError::PurchaseNotReturnable => PurchaseReturnsError::PurchaseNotReturnable,
        RepoError::SupplierBusy => PurchaseReturnsError::SupplierBusy,
        RepoError::SupplierInactive => PurchaseReturnsError::SupplierInactive,
        RepoError::ProductNotInPurchase { .. } => PurchaseReturnsError::ProductNotInPurchase,
        RepoError::ExceedsReturnable {
            product_name,
            available,
            requested,
        } => PurchaseReturnsError::ExceedsReturnable {
            product_name,
            available,
            requested,
        },
        RepoError::InsufficientStock {
            product_name,
            available,
            requested,
        } => PurchaseReturnsError::InsufficientStock {
            product_name,
            available,
            requested,
        },
        RepoError::ReturnExceedsPurchase {
            purchase_total,
            already_returned,
            requested,
        } => PurchaseReturnsError::ReturnExceedsPurchase {
            purchase_total,
            already_returned,
            requested,
        },
        RepoError::InsufficientWalletRefundCapacity {
            requested,
            available,
        } => PurchaseReturnsError::InsufficientWalletRefundCapacity {
            requested,
            available,
        },
        other => PurchaseReturnsError::Internal(other),
    }
}

pub async fn for_purchase(
    pool: &PgPool,
    purchase_id: Uuid,
) -> Result<PurchaseReturnsSummary, PurchaseReturnsError> {
    repository::for_purchase(pool, purchase_id)
        .await
        .map_err(map_repo_error)?
        .ok_or(PurchaseReturnsError::PurchaseNotFound)
}

pub struct CreatePurchaseReturnInput<'a> {
    pub purchase_id: Uuid,
    pub actor: Uuid,
    pub reason: &'a str,
    pub lines: &'a [ReturnLine],
    pub surplus_mode: SurplusMode,
    pub idempotency_key: Option<&'a str>,
}

pub async fn create(
    pool: &PgPool,
    input: CreatePurchaseReturnInput<'_>,
) -> Result<CreatedPurchaseReturn, PurchaseReturnsError> {
    let reason = input.reason.trim();
    let idempotency_key = input.idempotency_key.map(str::trim);

    if reason.is_empty() {
        return Err(PurchaseReturnsError::ReasonMissing);
    }
    if reason.chars().count() > MAX_REASON_LEN {
        return Err(PurchaseReturnsError::ReasonTooLong);
    }
    if let Some(key) = idempotency_key {
        if key.is_empty() {
            return Err(PurchaseReturnsError::IdempotencyKeyMissing);
        }
        if key.chars().count() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(PurchaseReturnsError::IdempotencyKeyTooLong);
        }
    }
    if input.lines.is_empty() {
        return Err(PurchaseReturnsError::Empty);
    }

    let mut seen = HashSet::with_capacity(input.lines.len());
    for line in input.lines {
        if line.quantity <= Decimal::ZERO {
            return Err(PurchaseReturnsError::QuantityNotPositive);
        }
        if line.quantity.round_dp(3) != line.quantity {
            return Err(PurchaseReturnsError::QuantityPrecision);
        }
        if !seen.insert(line.product_id) {
            return Err(PurchaseReturnsError::DuplicateProduct);
        }
    }

    repository::create(
        pool,
        NewPurchaseReturn {
            purchase_id: input.purchase_id,
            reason,
            lines: input.lines,
            surplus_mode: input.surplus_mode,
            created_by: input.actor,
            idempotency_key,
        },
    )
    .await
    .map_err(map_repo_error)
}
