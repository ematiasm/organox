//! Customer data access.
//!
//! SQL lives here and nowhere else. Two details are decisions rather than
//! accidents:
//!
//!  * the list is paginated with `COUNT(*) OVER ()`, the pattern that let the
//!    product list report its real total (issue #22);
//!  * `update` uses `COALESCE($n, column)`, so an absent field is untouched;
//!    the credit limit is the one field with a clear marker, because it is
//!    the only one a real case has asked to clear.

use rust_decimal::Decimal;
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
    /// `None` is unlimited, the column default.
    pub credit_limit: Option<Decimal>,
}

pub async fn create_customer(pool: &PgPool, new: NewCustomer<'_>) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO customers (id, name, tax_id, email, phone, credit_limit)
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

/// The unique index is on `upper(tax_id)`, and Postgres reports a unique-index
/// violation under the index name, so the mapping is stable.
fn is_tax_id_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.constraint() == Some("customers_tax_id_unique"))
}

/// A page of customers plus how many the filters match in total.
///
/// `pattern` is a finished `LIKE` pattern built by [`crate::search::contains`],
/// not the operator's text: the escaping happens once, in the service, and this
/// layer only binds it. A raw string here would silently lose substring
/// matching.
///
/// `total` is what lets the screen tell a full page from the last one. Every row
/// of a page carries the same value; an empty page carries none and reports
/// `-1`, which the service resolves with a re-count.
pub async fn list_customers(
    pool: &PgPool,
    include_inactive: bool,
    pattern: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Customer>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH page AS (
            SELECT
                c.id,
                c.name,
                c.tax_id,
                c.email,
                c.phone,
                c.is_active,
                c.credit_limit,
                c.created_at,
                c.updated_at,
                COUNT(*) OVER () AS total
            FROM customers c
            WHERE ($1::bool OR c.is_active)
              -- One box at the counter: the operator types either a name or a
              -- document fragment and must not have to say which.
              AND (
                    $2::text IS NULL
                    OR c.name ILIKE $2::text ESCAPE E'\\'
                    OR c.tax_id ILIKE $2::text ESCAPE E'\\'
              )
            -- `lower(name)` is what `customers_name_lower_idx` indexes, and it is
            -- the order an operator scanning the list expects: byte order would
            -- file every capitalized name ahead of every lower-case one.
            ORDER BY lower(c.name), c.id
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
            (COALESCE(e.received, 0) - COALESCE(e.refunded, 0) - COALESCE(a.allocated, 0))
                ::numeric(14, 2) AS "available_credit!: Decimal",
            p.created_at,
            p.updated_at,
            p.total AS "total!"
        FROM page p
        LEFT JOIN LATERAL (
            SELECT
                SUM(amount) AS balance,
                SUM(CASE WHEN kind = 'receipt' THEN -amount ELSE 0 END) AS received,
                SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END) AS refunded
            FROM customer_account_entries
            WHERE customer_id = p.id
        ) e ON TRUE
        LEFT JOIN LATERAL (
            SELECT SUM(al.amount) AS allocated
            FROM customer_account_allocations al
            JOIN customer_account_entries en ON en.id = al.entry_id
            WHERE en.customer_id = p.id
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
        .map(|row| Customer {
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

/// The number of customers the same filters match, ignoring the page.
///
/// Only the empty-page case needs this: `COUNT(*) OVER ()` reports nothing when
/// the page holds no rows, and an offset past the end is real once a client pages
/// by `offset`. Answering 0 there would claim the registry is empty.
pub async fn count_customers(
    pool: &PgPool,
    include_inactive: bool,
    pattern: Option<&str>,
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
        pattern,
    )
    .fetch_one(pool)
    .await?;

    Ok(count.count)
}

pub async fn find_customer(pool: &PgPool, id: Uuid) -> Result<Option<Customer>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT
            c.id,
            c.name,
            c.tax_id,
            c.email,
            c.phone,
            c.is_active,
            c.credit_limit AS "credit_limit?: Decimal",
            COALESCE(e.balance, 0)::numeric(14, 2) AS "balance!: Decimal",
            (COALESCE(e.received, 0) - COALESCE(e.refunded, 0) - COALESCE(a.allocated, 0))
                ::numeric(14, 2) AS "available_credit!: Decimal",
            c.created_at,
            c.updated_at
        FROM customers c
        LEFT JOIN LATERAL (
            SELECT
                SUM(amount) AS balance,
                SUM(CASE WHEN kind = 'receipt' THEN -amount ELSE 0 END) AS received,
                SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END) AS refunded
            FROM customer_account_entries
            WHERE customer_id = c.id
        ) e ON TRUE
        LEFT JOIN LATERAL (
            SELECT SUM(al.amount) AS allocated
            FROM customer_account_allocations al
            JOIN customer_account_entries en ON en.id = al.entry_id
            WHERE en.customer_id = c.id
        ) a ON TRUE
        WHERE c.id = $1
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
        credit_limit: row.credit_limit,
        balance: row.balance,
        available_credit: row.available_credit,
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
    /// The one clearable field. `None` leaves the limit untouched,
    /// `Some(None)` sets it back to unlimited, `Some(Some(x))` sets it to `x`.
    pub credit_limit: Option<Option<Decimal>>,
}

/// Update only the fields that are present.
///
/// `NULL` means "do not touch". It therefore also means a text field cannot
/// be cleared through this route: that would need an explicit marker, and no
/// real case has asked for one. The credit limit is the exception, because a
/// real case has: an operator who raises a limit to cover a good month must be
/// able to return the customer to unlimited without re-creating the row. Its
/// clear is a separate boolean flag, so `COALESCE` — which cannot write a NULL
/// on purpose — keeps guarding every other field exactly as before.
pub async fn update_customer(
    pool: &PgPool,
    id: Uuid,
    update: CustomerUpdate<'_>,
) -> Result<(), RepoError> {
    update_customer_in(pool, id, update).await
}

/// [`update_customer`] over an open connection or transaction, used when a
/// deactivation through `PATCH` must hold the customer account lock alongside
/// the balance check.
pub async fn update_customer_in<'e, E>(
    executor: E,
    id: Uuid,
    update: CustomerUpdate<'_>,
) -> Result<(), RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query!(
        r#"
        UPDATE customers
        SET name      = COALESCE($2, name),
            tax_id    = COALESCE($3, tax_id),
            email     = COALESCE($4, email),
            phone     = COALESCE($5, phone),
            is_active = COALESCE($6, is_active),
            credit_limit = CASE WHEN $7::bool THEN $8 ELSE credit_limit END
        WHERE id = $1
        "#,
        id,
        update.name,
        update.tax_id,
        update.email,
        update.phone,
        update.is_active,
        // The flag says a limit decision is being made in this request; the
        // value carries which one, `None` meaning "back to unlimited".
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

/// Deactivate a customer. Never delete: sales and credit notes will point here.
///
/// Returns 404 if nonexistent, and succeeds without touching anything if it is
/// already inactive.
pub async fn deactivate_customer(pool: &PgPool, id: Uuid) -> Result<(), RepoError> {
    deactivate_customer_in(pool, id).await
}

/// [`deactivate_customer`] inside an open transaction so the service can hold
/// the customer advisory lock while checking for an open account position.
pub async fn deactivate_customer_in<'e, E>(executor: E, id: Uuid) -> Result<(), RepoError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query!("UPDATE customers SET is_active = false WHERE id = $1", id)
        .execute(executor)
        .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }
    Ok(())
}
