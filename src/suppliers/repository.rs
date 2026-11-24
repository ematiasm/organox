//! Supplier data access.
//!
//! SQL lives here and nowhere else. Pagination uses `COUNT(*) OVER ()` with an
//! empty-page re-count fallback in the service so an offset past the end never
//! claims the registry is empty. Account balances and available credit are
//! computed via LATERAL joins against `supplier_account_entries` and
//! `supplier_account_allocations`.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::Supplier;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("supplier not found")]
    NotFound,

    #[error("the document is already in use")]
    TaxIdTaken,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewSupplier<'a> {
    pub name: &'a str,
    pub tax_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub phone: Option<&'a str>,
    pub credit_limit: Option<Decimal>,
}

pub async fn create_supplier(pool: &PgPool, new: NewSupplier<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO suppliers (id, name, tax_id, email, phone, credit_limit)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        id,
        new.name,
        new.tax_id,
        new.email,
        new.phone,
        new.credit_limit,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if is_tax_id_violation(&err) {
            RepoError::TaxIdTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

fn is_tax_id_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("suppliers_tax_id_unique"))
}

/// A page of suppliers plus how many rows match the filters in total.
///
/// `pattern` is an already-escaped `LIKE` pattern from [`crate::search::contains`].
/// Returns `-1` as total when the page is empty so the service can re-count.
pub async fn list_suppliers(
    pool: &PgPool,
    include_inactive: bool,
    pattern: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Supplier>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH page AS (
            SELECT
                s.id,
                s.name,
                s.tax_id,
                s.email,
                s.phone,
                s.is_active,
                s.credit_limit,
                s.created_at,
                s.updated_at,
                COUNT(*) OVER () AS total
            FROM suppliers s
            WHERE ($1::bool OR s.is_active)
              AND (
                    $2::text IS NULL
                    OR s.name ILIKE $2::text ESCAPE E'\\'
                    OR s.tax_id ILIKE $2::text ESCAPE E'\\'
              )
            ORDER BY lower(s.name), s.id
            LIMIT $3 OFFSET $4
        )
        SELECT
            p.id,
            p.name,
            p.tax_id,
            p.email,
            p.phone,
            p.is_active,
            p.credit_limit AS "credit_limit?: Decimal",
            COALESCE(e.balance, 0)::numeric(14, 2) AS "balance!: Decimal",
            (COALESCE(e.paid, 0) - COALESCE(e.refunded, 0) - COALESCE(a.allocated, 0))
                ::numeric(14, 2) AS "available_credit!: Decimal",
            p.created_at,
            p.updated_at,
            p.total AS "total!"
        FROM page p
        LEFT JOIN LATERAL (
            SELECT
                SUM(amount) AS balance,
                SUM(CASE WHEN kind IN ('payment', 'purchase_return_credit') THEN -amount ELSE 0 END) AS paid,
                SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END) AS refunded
            FROM supplier_account_entries
            WHERE supplier_id = p.id
        ) e ON TRUE
        LEFT JOIN LATERAL (
            SELECT SUM(al.amount) AS allocated
            FROM supplier_account_allocations al
            JOIN supplier_account_entries en ON en.id = al.entry_id
            WHERE en.supplier_id = p.id
        ) a ON TRUE
        ORDER BY lower(p.name), p.id
        "#,
        include_inactive,
        pattern,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = rows.first().map(|row| row.total).unwrap_or(-1);
    let items = rows
        .into_iter()
        .map(|row| Supplier {
            id: row.id,
            name: row.name,
            tax_id: row.tax_id,
            email: row.email,
            phone: row.phone,
            is_active: row.is_active,
            credit_limit: row.credit_limit,
            balance: row.balance,
            available_credit: row.available_credit,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect();

    Ok((items, total))
}

pub async fn count_suppliers(
    pool: &PgPool,
    include_inactive: bool,
    pattern: Option<&str>,
) -> Result<i64, RepoError> {
    let count = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM suppliers s
        WHERE ($1::bool OR s.is_active)
          AND (
                $2::text IS NULL
                OR s.name ILIKE $2::text ESCAPE E'\\'
                OR s.tax_id ILIKE $2::text ESCAPE E'\\'
          )
        "#,
        include_inactive,
        pattern,
    )
    .fetch_one(pool)
    .await?;

    Ok(count.count)
}

pub async fn find_supplier(pool: &PgPool, id: Uuid) -> Result<Option<Supplier>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT
            s.id,
            s.name,
            s.tax_id,
            s.email,
            s.phone,
            s.is_active,
            s.credit_limit AS "credit_limit?: Decimal",
            COALESCE(e.balance, 0)::numeric(14, 2) AS "balance!: Decimal",
            (COALESCE(e.paid, 0) - COALESCE(e.refunded, 0) - COALESCE(a.allocated, 0))
                ::numeric(14, 2) AS "available_credit!: Decimal",
            s.created_at,
            s.updated_at
        FROM suppliers s
        LEFT JOIN LATERAL (
            SELECT
                SUM(amount) AS balance,
                SUM(CASE WHEN kind IN ('payment', 'purchase_return_credit') THEN -amount ELSE 0 END) AS paid,
                SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END) AS refunded
            FROM supplier_account_entries
            WHERE supplier_id = s.id
        ) e ON TRUE
        LEFT JOIN LATERAL (
            SELECT SUM(al.amount) AS allocated
            FROM supplier_account_allocations al
            JOIN supplier_account_entries en ON en.id = al.entry_id
            WHERE en.supplier_id = s.id
        ) a ON TRUE
        WHERE s.id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Supplier {
        id: row.id,
        name: row.name,
        tax_id: row.tax_id,
        email: row.email,
        phone: row.phone,
        is_active: row.is_active,
        credit_limit: row.credit_limit,
        balance: row.balance,
        available_credit: row.available_credit,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

pub struct SupplierUpdate<'a> {
    pub name: Option<&'a str>,
    pub tax_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub phone: Option<&'a str>,
    pub is_active: Option<bool>,
    /// Tri-state: `None` = unchanged, `Some(None)` = unlimited, `Some(Some(x))` = cap.
    pub credit_limit: Option<Option<Decimal>>,
}

/// Update only the fields that are present.
pub async fn update_supplier(
    pool: &PgPool,
    id: Uuid,
    update: SupplierUpdate<'_>,
) -> Result<(), RepoError> {
    update_supplier_in(pool, id, update).await
}

/// [`update_supplier`] over an open connection or transaction (used when
/// deactivation via `PATCH` holds the supplier advisory lock).
pub async fn update_supplier_in<'e, E>(
    executor: E,
    id: Uuid,
    update: SupplierUpdate<'_>,
) -> Result<(), RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query!(
        r#"
        UPDATE suppliers
        SET name         = COALESCE($2, name),
            tax_id       = COALESCE($3, tax_id),
            email        = COALESCE($4, email),
            phone        = COALESCE($5, phone),
            is_active    = COALESCE($6, is_active),
            credit_limit = CASE WHEN $7::bool THEN $8 ELSE credit_limit END
        WHERE id = $1
        "#,
        id,
        update.name,
        update.tax_id,
        update.email,
        update.phone,
        update.is_active,
        update.credit_limit.is_some(),
        update.credit_limit.flatten(),
    )
    .execute(executor)
    .await
    .map_err(|err| {
        if is_tax_id_violation(&err) {
            RepoError::TaxIdTaken
        } else {
            RepoError::Database(err)
        }
    })?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}

/// Soft-deactivate a supplier. Idempotent; returns `NotFound` when unknown.
pub async fn deactivate_supplier(pool: &PgPool, id: Uuid) -> Result<(), RepoError> {
    deactivate_supplier_in(pool, id).await
}

/// [`deactivate_supplier`] inside an open transaction so the service can hold
/// the supplier advisory lock while checking for an open account position.
pub async fn deactivate_supplier_in<'e, E>(executor: E, id: Uuid) -> Result<(), RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query!("UPDATE suppliers SET is_active = false WHERE id = $1", id)
        .execute(executor)
        .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}
