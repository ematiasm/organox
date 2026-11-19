//! Credit-note use cases.
//!
//! Rules that live here and nowhere else:
//!  * only a completed sale can be credited;
//!  * the reason is required and bounded;
//!  * a line is positive, precise to the column, and unique per product;
//!  * the total is calculated from the sale's own prices, never sent by the
//!    client.

use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;

use super::model::{CreditNoteDetail, MAX_REASON_LEN, ReturnLine};
use super::repository::{self, NewCreditNote, RepoError, SaleCreditSummary};

#[derive(Debug, thiserror::Error)]
pub enum CreditNotesError {
    #[error("sale not found")]
    SaleNotFound,

    #[error("the sale is not completed: only a charged sale can be credited")]
    SaleNotCreditable,

    #[error("a credit note needs at least one line")]
    Empty,

    #[error("the quantity must be greater than zero")]
    QuantityNotPositive,

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("a product appears twice in the same credit note")]
    DuplicateProduct,

    #[error("the reason is required")]
    ReasonMissing,

    #[error("the reason is longer than {MAX_REASON_LEN} characters")]
    ReasonTooLong,

    #[error("the product is not a line of this sale")]
    ProductNotInSale,

    #[error("cannot return {requested} of {product_name}: only {available} left to return")]
    ExceedsReturnable {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("internal credit-note error")]
    Internal(#[from] RepoError),
}

fn map_repo_error(err: RepoError) -> CreditNotesError {
    match err {
        RepoError::SaleNotFound => CreditNotesError::SaleNotFound,
        RepoError::SaleNotCreditable => CreditNotesError::SaleNotCreditable,
        RepoError::ProductNotInSale { .. } => CreditNotesError::ProductNotInSale,
        RepoError::ExceedsReturnable {
            product_name,
            available,
            requested,
        } => CreditNotesError::ExceedsReturnable {
            product_name,
            available,
            requested,
        },
        other => CreditNotesError::Internal(other),
    }
}

/// What has already been credited on a sale, for the return screen.
pub async fn for_sale(pool: &PgPool, sale_id: Uuid) -> Result<SaleCreditSummary, CreditNotesError> {
    repository::for_sale(pool, sale_id)
        .await
        .map_err(map_repo_error)?
        .ok_or(CreditNotesError::SaleNotFound)
}

/// Issue a credit note for a charged sale.
///
/// Validate before touching the database: the shape of the request is a client
/// error (422) and must cost no query. What only the database can answer —
/// whether the sale is charged, whether the product is a line of it, how much
/// is still returnable — is checked inside the transaction.
pub async fn create(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
    reason: &str,
    lines: &[ReturnLine],
) -> Result<CreditNoteDetail, CreditNotesError> {
    let reason = reason.trim();

    if reason.is_empty() {
        return Err(CreditNotesError::ReasonMissing);
    }
    if reason.chars().count() > MAX_REASON_LEN {
        return Err(CreditNotesError::ReasonTooLong);
    }
    if lines.is_empty() {
        return Err(CreditNotesError::Empty);
    }

    let mut seen = HashSet::with_capacity(lines.len());
    for line in lines {
        if line.quantity <= Decimal::ZERO {
            return Err(CreditNotesError::QuantityNotPositive);
        }
        // Rejected, not silently rounded: `numeric(12,3)` cannot hold it and
        // returning 1.0004 as 1.000 would quietly change the request.
        if line.quantity.round_dp(3) != line.quantity {
            return Err(CreditNotesError::QuantityPrecision);
        }
        if !seen.insert(line.product_id) {
            return Err(CreditNotesError::DuplicateProduct);
        }
    }

    repository::create(
        pool,
        NewCreditNote {
            sale_id,
            reason,
            lines,
            created_by: actor,
        },
    )
    .await
    .map_err(map_repo_error)
}
