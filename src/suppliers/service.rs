//! Supplier use cases.
//!
//! Business rules:
//!  * a supplier is deactivated, never deleted, because purchases reference it;
//!  * `tax_id` is unique when present, compared without case;
//!  * paginated reads report honest totals even when `offset` exceeds total.

use sqlx::PgPool;
use uuid::Uuid;

use super::model::Supplier;
use super::repository::{self, NewSupplier, RepoError, SupplierUpdate};
use super::validation::{self, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum SupplierError {
    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("the document is already in use")]
    TaxIdTaken,

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
}

pub async fn create_supplier(
    pool: &PgPool,
    input: CreateSupplierInput,
) -> Result<Uuid, SupplierError> {
    let name = validation::validate_name(&input.name)?;
    let tax_id = validation::validate_tax_id(input.tax_id.as_deref())?;
    let email = validation::validate_email(input.email.as_deref())?;
    let phone = validation::validate_phone(input.phone.as_deref())?;

    let id = repository::create_supplier(
        pool,
        NewSupplier {
            name: &name,
            tax_id: tax_id.as_deref(),
            email: email.as_deref(),
            phone: phone.as_deref(),
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

    repository::update_supplier(
        pool,
        id,
        SupplierUpdate {
            name: name.as_deref(),
            tax_id: tax_id.as_deref(),
            email: email.as_deref(),
            phone: phone.as_deref(),
            is_active: input.is_active,
        },
    )
    .await
    .map_err(map_repo_error)
}

pub async fn deactivate_supplier(pool: &PgPool, id: Uuid) -> Result<(), SupplierError> {
    repository::deactivate_supplier(pool, id)
        .await
        .map_err(map_repo_error)
}

fn map_repo_error(err: RepoError) -> SupplierError {
    match err {
        RepoError::NotFound => SupplierError::NotFound,
        RepoError::TaxIdTaken => SupplierError::TaxIdTaken,
        other => SupplierError::Internal(other),
    }
}
