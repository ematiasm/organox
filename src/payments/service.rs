//! Payments use cases: validation, then the repository.
//!
//! The rules live here rather than in the handlers so the HTTP layer stays a
//! translator, and so the sale completion (which is not an HTTP request) can
//! reuse the same validation.

use rust_decimal::Decimal;
use uuid::Uuid;

use super::model::{PaymentMethod, WalletMovement, WalletMovementKind, WalletWithBalance};
use super::repository::{self, RepoError};

/// Long enough for "Cuenta corriente Banco Nación", short enough that it still
/// fits a row in a settings table. The DB only rejects blanks, so this bound is
/// the app's business.
const MAX_NAME_LEN: usize = 60;

/// `numeric(14, 2)` cannot hold more than this; validating here turns a database
/// overflow (a 500) into a 422 that names the problem.
fn max_amount() -> Decimal {
    // 12 integer digits and 2 decimals, the exact capacity of numeric(14, 2).
    Decimal::from_i128_with_scale(99_999_999_999_999, 2)
}

#[derive(Debug, thiserror::Error)]
pub enum PaymentsError {
    #[error("wallet not found")]
    WalletNotFound,

    #[error("payment method not found")]
    MethodNotFound,

    #[error("the name is already in use")]
    NameTaken,

    #[error("{0}")]
    Validation(String),

    #[error("internal: {0}")]
    Internal(RepoError),
}

impl From<RepoError> for PaymentsError {
    /// Domain-relevant repository failures keep their identity so the handler
    /// can answer 404 or 409 instead of a blanket 500.
    fn from(err: RepoError) -> Self {
        match err {
            RepoError::WalletNotFound => PaymentsError::WalletNotFound,
            RepoError::MethodNotFound => PaymentsError::MethodNotFound,
            RepoError::NameTaken => PaymentsError::NameTaken,
            RepoError::Database(_) => PaymentsError::Internal(err),
        }
    }
}

pub struct CreateWalletInput {
    pub name: String,
}

pub struct UpdateWalletInput {
    pub name: Option<String>,
    pub is_active: Option<bool>,
}

pub struct CreateMethodInput {
    pub name: String,
    pub wallet_id: Uuid,
}

pub struct UpdateMethodInput {
    pub name: Option<String>,
    pub wallet_id: Option<Uuid>,
    pub is_active: Option<bool>,
}

pub struct ManualMovementInput {
    pub kind: WalletMovementKind,
    pub amount: Decimal,
    pub note: Option<String>,
    pub actor_id: Uuid,
}

fn clean_name(raw: &str) -> Result<String, PaymentsError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(PaymentsError::Validation(
            "the name must not be blank".to_string(),
        ));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(PaymentsError::Validation(format!(
            "the name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    Ok(name.to_string())
}

pub async fn create_wallet(
    pool: &sqlx::PgPool,
    input: CreateWalletInput,
) -> Result<Uuid, PaymentsError> {
    let name = clean_name(&input.name)?;
    Ok(repository::create_wallet(pool, &name).await?)
}

pub async fn update_wallet(
    pool: &sqlx::PgPool,
    id: Uuid,
    input: UpdateWalletInput,
) -> Result<(), PaymentsError> {
    let name = input.name.map(|raw| clean_name(&raw)).transpose()?;
    repository::update_wallet(
        pool,
        id,
        repository::WalletUpdate {
            name: name.as_deref(),
            is_active: input.is_active,
        },
    )
    .await?;
    Ok(())
}

pub async fn list_wallets(
    pool: &sqlx::PgPool,
    include_inactive: bool,
) -> Result<Vec<WalletWithBalance>, PaymentsError> {
    Ok(repository::list_wallets(pool, include_inactive).await?)
}

pub async fn get_wallet(pool: &sqlx::PgPool, id: Uuid) -> Result<WalletWithBalance, PaymentsError> {
    Ok(repository::get_wallet(pool, id).await?)
}

pub async fn create_method(
    pool: &sqlx::PgPool,
    input: CreateMethodInput,
) -> Result<Uuid, PaymentsError> {
    let name = clean_name(&input.name)?;
    Ok(repository::create_method(
        pool,
        repository::NewMethod {
            name: &name,
            wallet_id: input.wallet_id,
        },
    )
    .await?)
}

pub async fn update_method(
    pool: &sqlx::PgPool,
    id: Uuid,
    input: UpdateMethodInput,
) -> Result<(), PaymentsError> {
    let name = input.name.map(|raw| clean_name(&raw)).transpose()?;
    repository::update_method(
        pool,
        id,
        repository::MethodUpdate {
            name: name.as_deref(),
            wallet_id: input.wallet_id,
            is_active: input.is_active,
        },
    )
    .await?;
    Ok(())
}

pub async fn list_methods(
    pool: &sqlx::PgPool,
    include_inactive: bool,
) -> Result<Vec<PaymentMethod>, PaymentsError> {
    Ok(repository::list_methods(pool, include_inactive).await?)
}

pub async fn list_movements(
    pool: &sqlx::PgPool,
    wallet_id: Uuid,
    limit: i64,
) -> Result<Vec<WalletMovement>, PaymentsError> {
    // Proving the wallet exists first turns a typo in the id into a 404 instead
    // of an empty list that looks like "no movements yet".
    repository::get_wallet(pool, wallet_id).await?;
    Ok(repository::list_movements(pool, wallet_id, limit).await?)
}

/// Record money moving by hand: an expense, or an adjustment for a mistake.
///
/// A `sale_payment` is rejected here on purpose. That money enters the ledger
/// only by charging a sale, which is the only path that also deducts stock and
/// freezes a total. `account_payment` and `account_payment_refund` are rejected
/// for the same reason: each is written only by the account endpoint that also
/// records its ledger counterpart, never letting the wallet and the debt ledger
/// disagree.
pub async fn record_manual_movement(
    pool: &sqlx::PgPool,
    wallet_id: Uuid,
    input: ManualMovementInput,
) -> Result<Uuid, PaymentsError> {
    if !input.kind.is_manual() {
        return Err(PaymentsError::Validation(
            "a sale payment is recorded by charging a sale, not by hand".to_string(),
        ));
    }

    if input.amount <= Decimal::ZERO {
        return Err(PaymentsError::Validation(
            "the amount must be greater than zero".to_string(),
        ));
    }

    // Rejected, not silently rounded: asking for 10.005 and storing 10.00 would
    // change the request without saying so. Same rule the catalog applies to
    // prices.
    if input.amount.round_dp(2) != input.amount {
        return Err(PaymentsError::Validation(
            "the amount has more than 2 decimal places".to_string(),
        ));
    }
    let amount = input.amount;

    if amount > max_amount() {
        return Err(PaymentsError::Validation(format!(
            "the amount must be at most {}",
            max_amount()
        )));
    }

    let note = input
        .note
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());

    Ok(repository::insert_movement(
        pool,
        repository::NewWalletMovement {
            wallet_id,
            kind: input.kind,
            amount,
            sale_id: None,
            payment_method_id: None,
            credit_note_id: None,
            // Manual movements never belong to a customer's account: the
            // database constraint refuses a `customer_id` on any kind except
            // `account_payment` and `account_payment_refund`, which this route
            // must keep rejecting.
            customer_id: None,
            note: note.as_deref(),
            created_by: Some(input.actor_id),
        },
    )
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn a_blank_name_is_rejected() {
        assert!(clean_name("   ").is_err());
    }

    #[test]
    fn a_name_is_trimmed() {
        assert_eq!(clean_name("  Caja  ").unwrap(), "Caja");
    }

    #[test]
    fn an_overlong_name_is_rejected() {
        assert!(clean_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn the_largest_amount_the_column_holds_is_accepted() {
        assert!(dec!(999999999999.99) <= max_amount());
    }

    #[test]
    fn an_amount_beyond_the_column_is_rejected() {
        assert!(dec!(1000000000000.00) > max_amount());
    }
}
