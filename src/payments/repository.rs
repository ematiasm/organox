//! Payments data access.
//!
//! The important query here is the balance: a `SUM` over the wallet's
//! movements with the sign given by `kind`, served by
//! `wallet_movements (wallet_id, created_at DESC)`. There is no balance column
//! to keep in sync, which is what makes the number trustworthy.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{PaymentMethod, Wallet, WalletMovement, WalletMovementKind, WalletWithBalance};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("wallet not found")]
    WalletNotFound,

    #[error("payment method not found")]
    MethodNotFound,

    #[error("the name is already in use")]
    NameTaken,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// The constraint that rejected the row, when the error came from the database.
fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}

// --- Wallets ------------------------------------------------------------

pub async fn create_wallet(pool: &PgPool, name: &str) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!("INSERT INTO wallets (id, name) VALUES ($1, $2)", id, name)
        .execute(pool)
        .await
        .map_err(|err| {
            if violated_constraint(&err) == Some("wallets_name_lower_unique") {
                RepoError::NameTaken
            } else {
                RepoError::Database(err)
            }
        })?;

    Ok(id)
}

pub struct WalletUpdate<'a> {
    pub name: Option<&'a str>,
    pub is_active: Option<bool>,
}

/// Update only the fields that are present. `COALESCE($n, column)` makes `NULL`
/// mean "do not touch", the same convention `update_product` uses.
pub async fn update_wallet(
    pool: &PgPool,
    id: Uuid,
    update: WalletUpdate<'_>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE wallets
        SET name      = COALESCE($2, name),
            is_active = COALESCE($3, is_active)
        WHERE id = $1
        "#,
        id,
        update.name,
        update.is_active,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("wallets_name_lower_unique") {
            RepoError::NameTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    if result.rows_affected() == 0 {
        return Err(RepoError::WalletNotFound);
    }
    Ok(())
}

/// Wallets with their calculated balance, alphabetically.
///
/// No pagination: a business has a handful of wallets, and the settings screen
/// needs all of them at once. Movements are the table that grows.
pub async fn list_wallets(
    pool: &PgPool,
    include_inactive: bool,
) -> Result<Vec<WalletWithBalance>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT w.id,
               w.name,
               w.is_active,
               w.created_at,
               w.updated_at,
               COALESCE(SUM(
                   CASE m.kind
                       WHEN 'sale_payment' THEN m.amount
                       WHEN 'sale_refund'  THEN -m.amount
                       WHEN 'adjustment_in' THEN m.amount
                       WHEN 'expense' THEN -m.amount
                       WHEN 'adjustment_out' THEN -m.amount
                       ELSE 0
                   END
               ), 0)::numeric(14, 2) AS "balance!: Decimal"
        FROM wallets w
        LEFT JOIN wallet_movements m ON m.wallet_id = w.id
        WHERE $1 OR w.is_active
        GROUP BY w.id, w.name, w.is_active, w.created_at, w.updated_at
        ORDER BY lower(w.name)
        "#,
        include_inactive
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| WalletWithBalance {
            wallet: Wallet {
                id: row.id,
                name: row.name,
                is_active: row.is_active,
                created_at: row.created_at,
                updated_at: row.updated_at,
            },
            balance: row.balance,
        })
        .collect())
}

pub async fn get_wallet(pool: &PgPool, id: Uuid) -> Result<WalletWithBalance, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT w.id,
               w.name,
               w.is_active,
               w.created_at,
               w.updated_at,
               COALESCE(SUM(
                   CASE m.kind
                       WHEN 'sale_payment' THEN m.amount
                       WHEN 'sale_refund'  THEN -m.amount
                       WHEN 'adjustment_in' THEN m.amount
                       WHEN 'expense' THEN -m.amount
                       WHEN 'adjustment_out' THEN -m.amount
                       ELSE 0
                   END
               ), 0)::numeric(14, 2) AS "balance!: Decimal"
        FROM wallets w
        LEFT JOIN wallet_movements m ON m.wallet_id = w.id
        WHERE w.id = $1
        GROUP BY w.id, w.name, w.is_active, w.created_at, w.updated_at
        "#,
        id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::WalletNotFound)?;

    Ok(WalletWithBalance {
        wallet: Wallet {
            id: row.id,
            name: row.name,
            is_active: row.is_active,
            created_at: row.created_at,
            updated_at: row.updated_at,
        },
        balance: row.balance,
    })
}

// --- Payment methods ----------------------------------------------------

pub struct NewMethod<'a> {
    pub name: &'a str,
    pub wallet_id: Uuid,
}

pub async fn create_method(pool: &PgPool, new: NewMethod<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO payment_methods (id, name, wallet_id)
        VALUES ($1, $2, $3)
        "#,
        id,
        new.name,
        new.wallet_id,
    )
    .execute(pool)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("payment_methods_name_lower_unique") => RepoError::NameTaken,
        // The wallet does not exist. Translated so a constraint name never
        // reaches the client.
        Some("payment_methods_wallet_id_fkey") => RepoError::WalletNotFound,
        _ => RepoError::Database(err),
    })?;

    Ok(id)
}

pub struct MethodUpdate<'a> {
    pub name: Option<&'a str>,
    pub wallet_id: Option<Uuid>,
    pub is_active: Option<bool>,
}

pub async fn update_method(
    pool: &PgPool,
    id: Uuid,
    update: MethodUpdate<'_>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE payment_methods
        SET name      = COALESCE($2, name),
            wallet_id = COALESCE($3, wallet_id),
            is_active = COALESCE($4, is_active)
        WHERE id = $1
        "#,
        id,
        update.name,
        update.wallet_id,
        update.is_active,
    )
    .execute(pool)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("payment_methods_name_lower_unique") => RepoError::NameTaken,
        Some("payment_methods_wallet_id_fkey") => RepoError::WalletNotFound,
        _ => RepoError::Database(err),
    })?;

    if result.rows_affected() == 0 {
        return Err(RepoError::MethodNotFound);
    }
    Ok(())
}

pub async fn list_methods(
    pool: &PgPool,
    include_inactive: bool,
) -> Result<Vec<PaymentMethod>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT pm.id,
               pm.name,
               pm.wallet_id,
               w.name AS "wallet_name!",
               w.is_active AS "wallet_is_active!",
               pm.is_active,
               pm.created_at,
               pm.updated_at
        FROM payment_methods pm
        JOIN wallets w ON w.id = pm.wallet_id
        WHERE $1 OR pm.is_active
        ORDER BY lower(pm.name)
        "#,
        include_inactive
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PaymentMethod {
            id: row.id,
            name: row.name,
            wallet_id: row.wallet_id,
            wallet_name: row.wallet_name,
            wallet_is_active: row.wallet_is_active,
            is_active: row.is_active,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect())
}

/// Resolve a method inside a transaction, to know which wallet it pays into and
/// whether it may still be used.
pub async fn find_method<'e, E>(executor: E, id: Uuid) -> Result<Option<PaymentMethod>, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query!(
        r#"
        SELECT pm.id,
               pm.name,
               pm.wallet_id,
               w.name AS "wallet_name!",
               w.is_active AS "wallet_is_active!",
               pm.is_active,
               pm.created_at,
               pm.updated_at
        FROM payment_methods pm
        JOIN wallets w ON w.id = pm.wallet_id
        WHERE pm.id = $1
        "#,
        id
    )
    .fetch_optional(executor)
    .await?;

    Ok(row.map(|row| PaymentMethod {
        id: row.id,
        name: row.name,
        wallet_id: row.wallet_id,
        wallet_name: row.wallet_name,
        wallet_is_active: row.wallet_is_active,
        is_active: row.is_active,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

// --- Wallet movements ---------------------------------------------------

pub struct NewWalletMovement<'a> {
    pub wallet_id: Uuid,
    pub kind: WalletMovementKind,
    pub amount: Decimal,
    pub sale_id: Option<Uuid>,
    pub payment_method_id: Option<Uuid>,
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// Insert one ledger line.
///
/// Generic over the executor so the sale completion can pass its transaction:
/// the stock movements, the `sales` update and this insert must commit together
/// or not at all.
pub async fn insert_movement<'e, E>(
    executor: E,
    new: NewWalletMovement<'_>,
) -> Result<Uuid, RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, sale_id, payment_method_id, note, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        id,
        new.wallet_id,
        new.kind as WalletMovementKind,
        new.amount,
        new.sale_id,
        new.payment_method_id,
        new.note,
        new.created_by,
    )
    .execute(executor)
    .await
    .map_err(|err| match violated_constraint(&err) {
        Some("wallet_movements_wallet_id_fkey") => RepoError::WalletNotFound,
        Some("wallet_movements_payment_method_id_fkey") => RepoError::MethodNotFound,
        _ => RepoError::Database(err),
    })?;

    Ok(id)
}

pub async fn list_movements(
    pool: &PgPool,
    wallet_id: Uuid,
    limit: i64,
) -> Result<Vec<WalletMovement>, RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT id,
               wallet_id,
               kind AS "kind: WalletMovementKind",
               amount,
               sale_id,
               payment_method_id,
               note,
               created_by,
               created_at
        FROM wallet_movements
        WHERE wallet_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        "#,
        wallet_id,
        limit
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| WalletMovement {
            id: row.id,
            wallet_id: row.wallet_id,
            kind: row.kind,
            amount: row.amount,
            sale_id: row.sale_id,
            payment_method_id: row.payment_method_id,
            note: row.note,
            created_by: row.created_by,
            created_at: row.created_at,
        })
        .collect())
}
