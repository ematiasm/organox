//! Supplier use cases.
//!
//! Business rules:
//!  * a supplier is deactivated, never deleted, because purchases reference it;
//!  * a supplier with a non-zero account balance or unapplied credit cannot be
//!    deactivated (checked under the supplier advisory lock);
//!  * `tax_id` is unique when present, compared without case;
//!  * `credit_limit` is `None` (unlimited), `0` (cash only), or a positive cap;
//!  * paginated reads report honest totals even when `offset` exceeds total.

use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::supplier_accounts::repository as accounts;

use super::model::Supplier;
use super::repository::{self, NewSupplier, RepoError, SupplierUpdate};
use super::validation::{self, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum SupplierError {
    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("the document is already in use")]
    TaxIdTaken,

    #[error(
        "the supplier still has an open account position (balance {balance:.2}, unapplied credit {unapplied_credit:.2}) and cannot be deactivated"
    )]
    OpenAccountPosition {
        balance: Decimal,
        unapplied_credit: Decimal,
    },

    #[error("the supplier account is busy, retry")]
    AccountBusy,

    #[error("supplier not found")]
    NotFound,

    #[error("internal supplier error")]
    Internal(#[from] RepoError),
}

pub struct CreateSupplierInput {
    pub name: String,
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub credit_limit: Option<Decimal>,
}

pub async fn create_supplier(
    pool: &PgPool,
    input: CreateSupplierInput,
) -> Result<Uuid, SupplierError> {
    let name = validation::validate_name(&input.name)?;
    let tax_id = validation::validate_tax_id(input.tax_id.as_deref())?;
    let email = validation::validate_email(input.email.as_deref())?;
    let phone = validation::validate_phone(input.phone.as_deref())?;
    let credit_limit = input
        .credit_limit
        .map(validation::validate_credit_limit)
        .transpose()?;

    let id = repository::create_supplier(
        pool,
        NewSupplier {
            name: &name,
            tax_id: tax_id.as_deref(),
            email: email.as_deref(),
            phone: phone.as_deref(),
            credit_limit,
        },
    )
    .await
    .map_err(map_repo_error)?;

    Ok(id)
}

pub async fn list_suppliers(
    pool: &PgPool,
    include_inactive: bool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Supplier>, i64), SupplierError> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);

    let search = search.map(str::trim).filter(|value| !value.is_empty());
    let pattern = search.map(crate::search::contains);

    let (items, total) =
        repository::list_suppliers(pool, include_inactive, pattern.as_deref(), limit, offset)
            .await
            .map_err(map_repo_error)?;

    let total = if total < 0 {
        repository::count_suppliers(pool, include_inactive, pattern.as_deref())
            .await
            .map_err(map_repo_error)?
    } else {
        total
    };

    Ok((items, total))
}

pub async fn get_supplier(pool: &PgPool, id: Uuid) -> Result<Supplier, SupplierError> {
    repository::find_supplier(pool, id)
        .await
        .map_err(map_repo_error)?
        .ok_or(SupplierError::NotFound)
}

pub struct UpdateSupplierInput {
    pub name: Option<String>,
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: Option<bool>,
    /// `None` leaves untouched, `Some(None)` clears to unlimited, `Some(Some(x))` sets cap.
    pub credit_limit: Option<Option<Decimal>>,
}

pub async fn update_supplier(
    pool: &PgPool,
    id: Uuid,
    input: UpdateSupplierInput,
) -> Result<(), SupplierError> {
    let name = input
        .name
        .as_deref()
        .map(validation::validate_name)
        .transpose()?;
    let tax_id = match input.tax_id.as_deref() {
        Some(raw) => validation::validate_tax_id(Some(raw))?,
        None => None,
    };
    let email = match input.email.as_deref() {
        Some(raw) => validation::validate_email(Some(raw))?,
        None => None,
    };
    let phone = match input.phone.as_deref() {
        Some(raw) => validation::validate_phone(Some(raw))?,
        None => None,
    };
    let credit_limit = match input.credit_limit {
        Some(raw) => Some(raw.map(validation::validate_credit_limit).transpose()?),
        None => None,
    };

    let update = SupplierUpdate {
        name: name.as_deref(),
        tax_id: tax_id.as_deref(),
        email: email.as_deref(),
        phone: phone.as_deref(),
        is_active: input.is_active,
        credit_limit,
    };

    if input.is_active == Some(false) {
        let mut tx = pool.begin().await.map_err(RepoError::Database)?;
        ensure_zero_position_in(&mut tx, id).await?;
        repository::update_supplier_in(&mut *tx, id, update)
            .await
            .map_err(map_repo_error)?;
        tx.commit().await.map_err(RepoError::Database)?;
        return Ok(());
    }

    repository::update_supplier(pool, id, update)
        .await
        .map_err(map_repo_error)
}

pub async fn deactivate_supplier(pool: &PgPool, id: Uuid) -> Result<(), SupplierError> {
    let mut tx = pool.begin().await.map_err(RepoError::Database)?;
    ensure_zero_position_in(&mut tx, id).await?;
    repository::deactivate_supplier_in(&mut *tx, id)
        .await
        .map_err(map_repo_error)?;
    tx.commit().await.map_err(RepoError::Database)?;
    Ok(())
}

/// Acquire the supplier advisory lock first — matching `authorize_credit` —
/// and verify the account holds neither a balance nor unapplied credit before
/// deactivating.
async fn ensure_zero_position_in(conn: &mut PgConnection, id: Uuid) -> Result<(), SupplierError> {
    accounts::lock_supplier(conn, id)
        .await
        .map_err(map_account_error)?;
    let position = accounts::account_position_in(conn, id)
        .await
        .map_err(map_account_error)?;
    if !position.balance.is_zero() || !position.unapplied_credit.is_zero() {
        return Err(SupplierError::OpenAccountPosition {
            balance: position.balance,
            unapplied_credit: position.unapplied_credit,
        });
    }
    Ok(())
}

fn map_account_error(err: accounts::RepoError) -> SupplierError {
    match err {
        accounts::RepoError::SupplierNotFound => SupplierError::NotFound,
        accounts::RepoError::SupplierBusy => SupplierError::AccountBusy,
        accounts::RepoError::Database(db) => SupplierError::Internal(RepoError::Database(db)),
        other => SupplierError::Internal(RepoError::Database(sqlx::Error::Protocol(format!(
            "supplier accounts repository: {other}"
        )))),
    }
}

fn map_repo_error(err: RepoError) -> SupplierError {
    match err {
        RepoError::NotFound => SupplierError::NotFound,
        RepoError::TaxIdTaken => SupplierError::TaxIdTaken,
        other => SupplierError::Internal(other),
    }
}
