//! Supplier data access.
//!
//! SQL lives here and nowhere else. Pagination uses `COUNT(*) OVER ()` with an
//! empty-page re-count fallback in the service so an offset past the end never
//! claims the registry is empty.

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
}

pub async fn create_supplier(pool: &PgPool, new: NewSupplier<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO suppliers (id, name, tax_id, email, phone)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id,
        new.name,
        new.tax_id,
        new.email,
        new.phone,
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
        SELECT
            s.id,
            s.name,
            s.tax_id,
            s.email,
            s.phone,
            s.is_active,
            s.created_at,
            s.updated_at,
            COUNT(*) OVER () AS "total!"
        FROM suppliers s
        WHERE ($1::bool OR s.is_active)
          AND (
                $2::text IS NULL
                OR s.name ILIKE $2::text ESCAPE E'\\'
                OR s.tax_id ILIKE $2::text ESCAPE E'\\'
          )
        ORDER BY s.name, s.id
        LIMIT $3 OFFSET $4
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
            id,
            name,
            tax_id,
            email,
            phone,
            is_active,
            created_at,
            updated_at
        FROM suppliers
        WHERE id = $1
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
}

/// Update only the fields that are present (`COALESCE`).
pub async fn update_supplier(
    pool: &PgPool,
    id: Uuid,
    update: SupplierUpdate<'_>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE suppliers
        SET name      = COALESCE($2, name),
            tax_id    = COALESCE($3, tax_id),
            email     = COALESCE($4, email),
            phone     = COALESCE($5, phone),
            is_active = COALESCE($6, is_active)
        WHERE id = $1
        "#,
        id,
        update.name,
        update.tax_id,
        update.email,
        update.phone,
        update.is_active,
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

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}

/// Soft-deactivate a supplier. Idempotent; returns `NotFound` when unknown.
pub async fn deactivate_supplier(pool: &PgPool, id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!("UPDATE suppliers SET is_active = false WHERE id = $1", id)
        .execute(pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}
