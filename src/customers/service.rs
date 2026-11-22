//! Customer use cases.
//!
//! Business rules that live here and nowhere else:
//!  * a customer is deactivated, never deleted, because history points at it;
//!  * a document belongs to exactly one customer, compared without case;
//!  * a page never claims the registry is empty when it is not.

use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::model::Customer;
use super::repository::{self, CustomerUpdate, NewCustomer, RepoError};
use super::validation::{self, ValidationError};
use crate::customer_accounts::repository as accounts;

#[derive(Debug, thiserror::Error)]
pub enum CustomerError {
    #[error(transparent)]
    Validation(#[from] ValidationError),

    #[error("the document is already in use")]
    TaxIdTaken,

    #[error("{}", format_open_position(*.balance, *.unapplied_credit))]
    OpenAccountPosition {
        balance: Decimal,
        unapplied_credit: Decimal,
    },

    #[error("the customer account is busy, retry")]
    AccountBusy,

    #[error("customer not found")]
    NotFound,

    #[error("internal customer error")]
    Internal(#[from] RepoError),
}

fn format_open_position(balance: Decimal, unapplied_credit: Decimal) -> String {
    if !unapplied_credit.is_zero() {
        format!(
            "the customer cannot be deactivated with balance {balance:.2} and unapplied credit {unapplied_credit:.2}"
        )
    } else {
        format!("the customer cannot be deactivated with balance {balance:.2}")
    }
}

pub struct CreateCustomerInput {
    pub name: String,
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// `None` is unlimited; `0` is cash only. Validated here, never left to
    /// the column to round or reject.
    pub credit_limit: Option<Decimal>,
}

pub async fn create_customer(
    pool: &PgPool,
    input: CreateCustomerInput,
) -> Result<Uuid, CustomerError> {
    let name = validation::validate_name(&input.name)?;
    let tax_id = validation::validate_tax_id(input.tax_id.as_deref())?;
    let email = validation::validate_email(input.email.as_deref())?;
    let phone = validation::validate_phone(input.phone.as_deref())?;
    let credit_limit = input
        .credit_limit
        .map(validation::validate_credit_limit)
        .transpose()?;

    let id = repository::create_customer(
        pool,
        NewCustomer {
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
    // Escaped here, once, so the SQL binds a finished pattern: `%` and `_`
    // typed by an operator are text, not wildcards.
    let pattern = search.map(crate::search::contains);

    let (items, total) =
        repository::list_customers(pool, include_inactive, pattern.as_deref(), limit, offset)
            .await
            .map_err(map_repo_error)?;

    // An offset past the last customer returns an empty page, and an empty page
    // carries no `COUNT(*) OVER ()` value. Re-count so a stale link reports the
    // real total instead of claiming the registry is empty.
    let total = if total < 0 {
        repository::count_customers(pool, include_inactive, pattern.as_deref())
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
    /// `None` leaves the limit untouched, `Some(None)` clears it back to
    /// unlimited, `Some(Some(x))` sets it to `x`. The double `Option` is the
    /// same shape `PUT /sales/{id}/customer` uses for `customer_id`, for the
    /// same reason: serde collapses a missing field and an explicit `null`.
    pub credit_limit: Option<Option<Decimal>>,
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
    // Only a decision that is actually present is validated: an absent field
    // must not turn "untouched" into a rejection.
    let credit_limit = match input.credit_limit {
        Some(raw) => Some(raw.map(validation::validate_credit_limit).transpose()?),
        None => None,
    };

    let update = CustomerUpdate {
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
        repository::update_customer_in(&mut *tx, id, update)
            .await
            .map_err(map_repo_error)?;
        tx.commit().await.map_err(RepoError::Database)?;
        return Ok(());
    }

    repository::update_customer(pool, id, update)
        .await
        .map_err(map_repo_error)
}

pub async fn deactivate_customer(pool: &PgPool, id: Uuid) -> Result<(), CustomerError> {
    let mut tx = pool.begin().await.map_err(RepoError::Database)?;
    ensure_zero_position_in(&mut tx, id).await?;
    repository::deactivate_customer_in(&mut *tx, id)
        .await
        .map_err(map_repo_error)?;
    tx.commit().await.map_err(RepoError::Database)?;
    Ok(())
}

/// Take the customer's advisory lock first — the same lock order
/// `authorize_credit` uses — and verify the account carries neither a balance
/// nor unapplied credit before deactivating the row.
async fn ensure_zero_position_in(conn: &mut PgConnection, id: Uuid) -> Result<(), CustomerError> {
    accounts::lock_customer(conn, id)
        .await
        .map_err(map_account_error)?;
    let position = accounts::account_position_in(conn, id)
        .await
        .map_err(map_account_error)?;
    if !position.balance.is_zero() || !position.unapplied_credit.is_zero() {
        return Err(CustomerError::OpenAccountPosition {
            balance: position.balance,
            unapplied_credit: position.unapplied_credit,
        });
    }
    Ok(())
}

fn map_account_error(err: accounts::RepoError) -> CustomerError {
    match err {
        accounts::RepoError::CustomerNotFound => CustomerError::NotFound,
        accounts::RepoError::CustomerBusy => CustomerError::AccountBusy,
        accounts::RepoError::Database(db) => CustomerError::Internal(RepoError::Database(db)),
        other => CustomerError::Internal(RepoError::Database(sqlx::Error::Protocol(format!(
            "customer accounts repository: {other}"
        )))),
    }
}

fn map_repo_error(err: RepoError) -> CustomerError {
    match err {
        RepoError::NotFound => CustomerError::NotFound,
        RepoError::TaxIdTaken => CustomerError::TaxIdTaken,
        other => CustomerError::Internal(other),
    }
}
