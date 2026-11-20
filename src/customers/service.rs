//! Customer use cases.
//!
//! Business rules that live here and nowhere else:
//!  * a customer is deactivated, never deleted, because history points at it;
//!  * a document belongs to exactly one customer, compared without case;
//!  * a page never claims the registry is empty when it is not.

use sqlx::PgPool;
use uuid::Uuid;

use super::model::Customer;
use super::repository::{self, CustomerUpdate, NewCustomer, RepoError};
use super::validation::{self, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum CustomerError {
    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("the document is already in use")]
    TaxIdTaken,

    #[error("customer not found")]
    NotFound,

    #[error("internal customer error")]
    Internal(#[from] RepoError),
}

pub struct CreateCustomerInput {
    pub name: String,
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

pub async fn create_customer(
    pool: &PgPool,
    input: CreateCustomerInput,
) -> Result<Uuid, CustomerError> {
    let name = validation::validate_name(&input.name)?;
    let tax_id = validation::validate_tax_id(input.tax_id.as_deref())?;
    let email = validation::validate_email(input.email.as_deref())?;
    let phone = validation::validate_phone(input.phone.as_deref())?;

    let id = repository::create_customer(
        pool,
        NewCustomer {
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

/// A page of customers and how many the filters match in total.
pub async fn list_customers(
    pool: &PgPool,
    include_inactive: bool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Customer>, i64), CustomerError> {
    // Defensive cap: a `limit` of 10 million is a per-query DoS. The handler
    // clamps too, so the response can report the page size actually applied.
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);

    let search = search.map(str::trim).filter(|value| !value.is_empty());

    let (items, total) = repository::list_customers(pool, include_inactive, search, limit, offset)
        .await
        .map_err(map_repo_error)?;

    // An offset past the last customer returns an empty page, and an empty page
    // carries no `COUNT(*) OVER ()` value. Re-count so a stale link reports the
    // real total instead of claiming the registry is empty.
    let total = if total < 0 {
        repository::count_customers(pool, include_inactive, search)
            .await
            .map_err(map_repo_error)?
    } else {
        total
    };

    Ok((items, total))
}

pub async fn get_customer(pool: &PgPool, id: Uuid) -> Result<Customer, CustomerError> {
    repository::find_customer(pool, id)
        .await
        .map_err(map_repo_error)?
        .ok_or(CustomerError::NotFound)
}

pub struct UpdateCustomerInput {
    pub name: Option<String>,
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: Option<bool>,
}

pub async fn update_customer(
    pool: &PgPool,
    id: Uuid,
    input: UpdateCustomerInput,
) -> Result<(), CustomerError> {
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

    repository::update_customer(
        pool,
        id,
        CustomerUpdate {
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

pub async fn deactivate_customer(pool: &PgPool, id: Uuid) -> Result<(), CustomerError> {
    repository::deactivate_customer(pool, id)
        .await
        .map_err(map_repo_error)
}

fn map_repo_error(err: RepoError) -> CustomerError {
    match err {
        RepoError::NotFound => CustomerError::NotFound,
        RepoError::TaxIdTaken => CustomerError::TaxIdTaken,
        other => CustomerError::Internal(other),
    }
}
