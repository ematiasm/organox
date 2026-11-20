//! Customer data access.
//!
//! SQL lives here and nowhere else. Two details are decisions rather than
//! accidents:
//!
//!  * the list is paginated with `COUNT(*) OVER ()`, the pattern that let the
//!    product list report its real total (issue #22);
//!  * `update` uses `COALESCE($n, column)`, so an absent field is untouched.

use sqlx::PgPool;
use uuid::Uuid;

use super::model::Customer;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("customer not found")]
    NotFound,

    #[error("the document is already in use")]
    TaxIdTaken,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewCustomer<'a> {
    pub name: &'a str,
    pub tax_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub phone: Option<&'a str>,
}

pub async fn create_customer(pool: &PgPool, new: NewCustomer<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO customers (id, name, tax_id, email, phone)
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

/// The unique index is on `upper(tax_id)`, and Postgres reports a unique-index
/// violation under the index name, so the mapping is stable.
fn is_tax_id_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("customers_tax_id_unique"))
}

/// A page of customers plus how many the filters match in total.
///
/// `total` is what lets the screen tell a full page from the last one. Every row
/// of a page carries the same value; an empty page carries none and reports
/// `-1`, which the service resolves with a re-count.
pub async fn list_customers(
    pool: &PgPool,
    include_inactive: bool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Customer>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT
            c.id,
            c.name,
            c.tax_id,
            c.email,
            c.phone,
            c.is_active,
            c.created_at,
            c.updated_at,
            COUNT(*) OVER () AS "total!"
        FROM customers c
        WHERE ($1::bool OR c.is_active)
          -- One box at the counter: the operator types either a name or a
          -- document fragment and must not have to say which.
          AND (
                $2::text IS NULL
                OR c.name ILIKE $2::text ESCAPE E'\\'
                OR c.tax_id ILIKE $2::text ESCAPE E'\\'
          )
        ORDER BY c.name, c.id
        LIMIT $3 OFFSET $4
        "#,
        include_inactive,
        search,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = rows.first().map(|row| row.total).unwrap_or(-1);
    let items = rows
        .into_iter()
        .map(|row| Customer {
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

/// The number of customers the same filters match, ignoring the page.
///
/// Only the empty-page case needs this: `COUNT(*) OVER ()` reports nothing when
/// the page holds no rows, and an offset past the end is real once a client pages
/// by `offset`. Answering 0 there would claim the registry is empty.
pub async fn count_customers(
    pool: &PgPool,
    include_inactive: bool,
    search: Option<&str>,
) -> Result<i64, RepoError> {
    let count = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM customers c
        WHERE ($1::bool OR c.is_active)
          AND (
                $2::text IS NULL
                OR c.name ILIKE $2::text ESCAPE E'\\'
                OR c.tax_id ILIKE $2::text ESCAPE E'\\'
          )
        "#,
        include_inactive,
        search,
    )
    .fetch_one(pool)
    .await?;

    Ok(count.count)
}

pub async fn find_customer(pool: &PgPool, id: Uuid) -> Result<Option<Customer>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT id, name, tax_id, email, phone, is_active, created_at, updated_at
        FROM customers
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Customer {
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

pub struct CustomerUpdate<'a> {
    pub name: Option<&'a str>,
    pub tax_id: Option<&'a str>,
    pub email: Option<&'a str>,
    pub phone: Option<&'a str>,
    pub is_active: Option<bool>,
}

/// Update only the fields that are present.
///
/// `NULL` means "do not touch". It therefore also means an optional field cannot
/// be cleared through this route: that needs an explicit marker, to be added when
/// a real case calls for it. The alternative — treating `NULL` as "clear" — makes
/// every partial update from the UI erase the fields the form did not send.
pub async fn update_customer(
    pool: &PgPool,
    id: Uuid,
    update: CustomerUpdate<'_>,
) -> Result<(), RepoError> {
    let result = sqlx::query!(
        r#"
        UPDATE customers
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

/// Deactivate a customer. Never delete: sales and credit notes will point here.
///
/// Returns 404 if nonexistent, and succeeds without touching anything if it is
/// already inactive.
pub async fn deactivate_customer(pool: &PgPool, id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!("UPDATE customers SET is_active = false WHERE id = $1", id)
        .execute(pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}
