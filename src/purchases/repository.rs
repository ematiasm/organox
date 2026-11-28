//! Purchases persistence.
//!
//! All draft mutations lock the purchase row (`FOR UPDATE`), and completion
//! runs in one transaction that locks the purchase, supplier, and products
//! before inserting `stock_movements`, `wallet_movements`, and freezing
//! `supplier_name` + `total`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use super::model::{Purchase, PurchaseItem, PurchasePaymentInput, PurchaseStatus};
use crate::catalog::model::ProductKind;
use crate::payments::model::WalletMovementKind;
use crate::payments::repository as payments;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("purchase not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("the product is deactivated and cannot be added to a purchase")]
    ProductInactive,

    #[error("a service does not track stock and cannot be added to a purchase")]
    ProductDoesNotTrackStock,

    #[error("the purchase is already closed and cannot be modified")]
    NotEditable,

    #[error("the purchase is empty and cannot be completed")]
    EmptyPurchase,

    #[error("a supplier is required to complete a purchase")]
    SupplierRequired,

    #[error("supplier not found")]
    SupplierNotFound,

    #[error("the supplier is deactivated and cannot be used for a purchase")]
    SupplierInactive,

    #[error("the quantity exceeds the allowed maximum")]
    QuantityTooLarge,

    #[error("the purchase total exceeds the allowed maximum")]
    TotalTooLarge,

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payment amount exceeds the allowed maximum")]
    PaymentAmountTooLarge,

    #[error("the payments do not add up to the purchase total")]
    PaymentMismatch {
        total: Decimal,
        paid: Decimal,
        on_account: Decimal,
    },

    #[error("the payment method is unknown or inactive")]
    PaymentMethodUnavailable,

    #[error("the on-account amount cannot be negative")]
    OnAccountNegative,

    #[error("the on-account amount has more than 2 decimal places")]
    OnAccountPrecision,

    #[error("the on-account amount exceeds the allowed maximum")]
    OnAccountTooLarge,

    #[error("the on-account amount exceeds the supplier's available credit")]
    OnAccountExceedsCreditLimit { available: Decimal },

    #[error("the supplier account is busy, retry")]
    SupplierAccountBusy,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

async fn find_open_draft_for_user(pool: &PgPool, actor: Uuid) -> Result<Option<Uuid>, RepoError> {
    let id = sqlx::query_scalar!(
        r#"
        SELECT id
        FROM purchases
        WHERE created_by = $1 AND status = 'draft'
        ORDER BY created_at DESC, id DESC
        LIMIT 1
        "#,
        actor
    )
    .fetch_optional(pool)
    .await?;

    Ok(id)
}

/// Reuse the operator's open draft, or create a new empty draft without racing
/// against concurrent `open_draft` calls.
///
/// Backed by the `purchases_one_open_draft_per_user_idx` partial unique index:
/// when concurrent callers race past the fast-path lookup, `ON CONFLICT ...
/// DO NOTHING` lets one insert win (`reused = false`) while losers reload the
/// winning draft (`reused = true`).
pub async fn open_or_find_draft(pool: &PgPool, actor: Uuid) -> Result<(Uuid, bool), RepoError> {
    if let Some(existing) = find_open_draft_for_user(pool, actor).await? {
        return Ok((existing, true));
    }

    for _ in 0..3 {
        let candidate_id = Uuid::now_v7();
        let inserted = sqlx::query_scalar!(
            r#"
            INSERT INTO purchases (id, status, created_by)
            VALUES ($1, 'draft', $2)
            ON CONFLICT (created_by) WHERE status = 'draft' DO NOTHING
            RETURNING id
            "#,
            candidate_id,
            actor,
        )
        .fetch_optional(pool)
        .await?;

        if let Some(id) = inserted {
            return Ok((id, false));
        }

        if let Some(existing) = find_open_draft_for_user(pool, actor).await? {
            return Ok((existing, true));
        }
    }

    Err(RepoError::Database(sqlx::Error::Protocol(
        "concurrent draft state changed repeatedly while opening purchase draft".to_string(),
    )))
}

async fn refresh_total_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    purchase_id: Uuid,
) -> Result<(), RepoError> {
    sqlx::query!(
        r#"
        UPDATE purchases
        SET total = COALESCE((
            SELECT SUM(ROUND(quantity * unit_cost, 2)) FROM purchase_items WHERE purchase_id = $1
        ), 0)
        WHERE id = $1
        "#,
        purchase_id
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Attach or clear the supplier on a draft inside a single transaction.
///
/// Locks the `purchases` row (`FOR NO KEY UPDATE`) before locking the
/// `suppliers` row (`FOR SHARE`), matching `complete()`'s lock order to
/// prevent deadlocks while serializing against concurrent supplier
/// deactivation.
pub async fn set_supplier(
    pool: &PgPool,
    purchase_id: Uuid,
    supplier_id: Option<Uuid>,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR NO KEY UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if status != PurchaseStatus::Draft {
        return Err(RepoError::NotEditable);
    }

    if let Some(sid) = supplier_id {
        let is_active = sqlx::query_scalar!(
            r#"SELECT is_active FROM suppliers WHERE id = $1 FOR SHARE"#,
            sid
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::SupplierNotFound)?;

        if !is_active {
            return Err(RepoError::SupplierInactive);
        }
    }

    sqlx::query!(
        r#"
        UPDATE purchases
        SET supplier_id = $2
        WHERE id = $1
        "#,
        purchase_id,
        supplier_id,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Add a product to a draft. If already present, sums quantity and updates `unit_cost`.
pub async fn upsert_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let product = sqlx::query!(
        r#"
        SELECT kind AS "kind: ProductKind", is_active
        FROM products
        WHERE id = $1
        "#,
        product_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::ProductNotFound)?;

    if !product.is_active {
        return Err(RepoError::ProductInactive);
    }
    if !product.kind.tracks_stock() {
        return Err(RepoError::ProductDoesNotTrackStock);
    }

    if quantity > crate::money::MAX_QUANTITY {
        return Err(RepoError::QuantityTooLarge);
    }

    // Validate the summed line quantity (`numeric(12, 3)`) and projected
    // purchase total (`numeric(14, 2)`) under `purchases FOR UPDATE` before
    // writing, turning Postgres `22003` numeric overflows into a typed 422.
    let existing_items = sqlx::query!(
        r#"
        SELECT product_id, quantity, unit_cost
        FROM purchase_items
        WHERE purchase_id = $1
        "#,
        purchase_id
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut found = false;
    let mut projected = Vec::with_capacity(existing_items.len() + 1);
    for item in existing_items {
        if item.product_id == product_id {
            let new_qty = crate::money::checked_add_quantity(item.quantity, quantity)
                .ok_or(RepoError::QuantityTooLarge)?;
            projected.push((new_qty, unit_cost));
            found = true;
        } else {
            projected.push((item.quantity, item.unit_cost));
        }
    }
    if !found {
        projected.push((quantity, unit_cost));
    }
    crate::money::checked_document_total(projected).ok_or(RepoError::TotalTooLarge)?;

    let item_id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (purchase_id, product_id)
        DO UPDATE SET
            quantity  = purchase_items.quantity + EXCLUDED.quantity,
            unit_cost = EXCLUDED.unit_cost
        "#,
        item_id,
        purchase_id,
        product_id,
        quantity,
        unit_cost,
    )
    .execute(&mut *tx)
    .await?;

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Replace quantity and unit cost of an existing item in a draft.
pub async fn set_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Result<(), RepoError> {
    if quantity > crate::money::MAX_QUANTITY {
        return Err(RepoError::QuantityTooLarge);
    }

    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let existing_items = sqlx::query!(
        r#"
        SELECT product_id, quantity, unit_cost
        FROM purchase_items
        WHERE purchase_id = $1
        "#,
        purchase_id
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut found = false;
    let mut projected = Vec::with_capacity(existing_items.len());
    for item in existing_items {
        if item.product_id == product_id {
            projected.push((quantity, unit_cost));
            found = true;
        } else {
            projected.push((item.quantity, item.unit_cost));
        }
    }
    if !found {
        return Err(RepoError::NotFound);
    }
    crate::money::checked_document_total(projected).ok_or(RepoError::TotalTooLarge)?;

    sqlx::query!(
        r#"
        UPDATE purchase_items
        SET quantity = $3, unit_cost = $4
        WHERE purchase_id = $1 AND product_id = $2
        "#,
        purchase_id,
        product_id,
        quantity,
        unit_cost,
    )
    .execute(&mut *tx)
    .await?;

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Remove a line from a draft.
pub async fn remove_item(
    pool: &PgPool,
    purchase_id: Uuid,
    product_id: Uuid,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: PurchaseStatus" FROM purchases WHERE id = $1 FOR UPDATE"#,
        purchase_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let result = sqlx::query!(
        "DELETE FROM purchase_items WHERE purchase_id = $1 AND product_id = $2",
        purchase_id,
        product_id,
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() == 0 {
        return Err(RepoError::NotFound);
    }

    refresh_total_in(&mut tx, purchase_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Cancel a draft purchase. Completed or already-cancelled purchases return `NotEditable`.
pub async fn cancel(pool: &PgPool, purchase_id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!(
        "UPDATE purchases SET status = 'cancelled' WHERE id = $1 AND status = 'draft'",
        purchase_id
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        let exists = sqlx::query_scalar!("SELECT 1 FROM purchases WHERE id = $1", purchase_id)
            .fetch_optional(pool)
            .await?
            .is_some();
        return Err(if exists {
            RepoError::NotEditable
        } else {
            RepoError::NotFound
        });
    }

    Ok(())
}

pub async fn find_purchase(
    pool: &PgPool,
    purchase_id: Uuid,
) -> Result<Option<Purchase>, RepoError> {
    let Some(row) = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.status AS "status: PurchaseStatus",
            p.total,
            p.supplier_id,
            COALESCE(p.supplier_name, s.name) AS supplier_name,
            p.note,
            p.created_by,
            p.created_at,
            p.completed_at
        FROM purchases p
        LEFT JOIN suppliers s ON s.id = p.supplier_id
        WHERE p.id = $1
        "#,
        purchase_id
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let items = sqlx::query!(
        r#"
        SELECT
            pi.id,
            pi.product_id,
            pr.name AS product_name,
            pi.quantity,
            pi.unit_cost
        FROM purchase_items pi
        JOIN products pr ON pr.id = pi.product_id
        WHERE pi.purchase_id = $1
        ORDER BY pi.created_at ASC, pi.id ASC
        "#,
        purchase_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|item| PurchaseItem {
        id: item.id,
        product_id: item.product_id,
        product_name: item.product_name,
        quantity: item.quantity,
        unit_cost: item.unit_cost,
    })
    .collect();

    Ok(Some(Purchase {
        id: row.id,
        status: row.status,
        items,
        frozen_total: if row.status == PurchaseStatus::Completed {
            row.total
        } else {
            None
        },
        supplier_id: row.supplier_id,
        supplier_name: row.supplier_name,
        note: row.note,
        created_by: row.created_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
    }))
}

pub async fn list_purchases(
    pool: &PgPool,
    status: Option<PurchaseStatus>,
    supplier_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Purchase>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT
            p.id,
            p.status AS "status: PurchaseStatus",
            p.total,
            p.supplier_id,
            COALESCE(p.supplier_name, s.name) AS supplier_name,
            p.note,
            p.created_by,
            p.created_at,
            p.completed_at,
            COUNT(*) OVER () AS "row_total!"
        FROM purchases p
        LEFT JOIN suppliers s ON s.id = p.supplier_id
        WHERE ($1::purchase_status IS NULL OR p.status = $1)
          AND ($2::uuid IS NULL OR p.supplier_id = $2)
        ORDER BY p.created_at DESC, p.id DESC
        LIMIT $3 OFFSET $4
        "#,
        status as Option<PurchaseStatus>,
        supplier_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = rows.first().map(|r| r.row_total).unwrap_or(-1);
    if rows.is_empty() {
        return Ok((Vec::new(), total));
    }

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let item_rows = sqlx::query!(
        r#"
        SELECT
            pi.id,
            pi.purchase_id,
            pi.product_id,
            pr.name AS product_name,
            pi.quantity,
            pi.unit_cost
        FROM purchase_items pi
        JOIN products pr ON pr.id = pi.product_id
        WHERE pi.purchase_id = ANY($1)
        ORDER BY pi.created_at ASC, pi.id ASC
        "#,
        &ids
    )
    .fetch_all(pool)
    .await?;

    let mut items_by_purchase: HashMap<Uuid, Vec<PurchaseItem>> = HashMap::new();
    for item in item_rows {
        items_by_purchase
            .entry(item.purchase_id)
            .or_default()
            .push(PurchaseItem {
                id: item.id,
                product_id: item.product_id,
                product_name: item.product_name,
                quantity: item.quantity,
                unit_cost: item.unit_cost,
            });
    }

    let items = rows
        .into_iter()
        .map(|row| Purchase {
            id: row.id,
            status: row.status,
            items: items_by_purchase.remove(&row.id).unwrap_or_default(),
            frozen_total: if row.status == PurchaseStatus::Completed {
                row.total
            } else {
                None
            },
            supplier_id: row.supplier_id,
            supplier_name: row.supplier_name,
            note: row.note,
            created_by: row.created_by,
            created_at: row.created_at,
            completed_at: row.completed_at,
        })
        .collect();

    Ok((items, total))
}

pub async fn count_purchases(
    pool: &PgPool,
    status: Option<PurchaseStatus>,
    supplier_id: Option<Uuid>,
) -> Result<i64, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM purchases p
        WHERE ($1::purchase_status IS NULL OR p.status = $1)
          AND ($2::uuid IS NULL OR p.supplier_id = $2)
        "#,
        status as Option<PurchaseStatus>,
        supplier_id,
    )
    .fetch_one(pool)
    .await?;

    Ok(row.count)
}

#[derive(Debug)]
pub struct CompletedPurchase {
    pub purchase_id: Uuid,
    pub total: Decimal,
    pub movement_ids: Vec<Uuid>,
    pub payment_movement_ids: Vec<Uuid>,
    pub credit_applied: Decimal,
    pub completed_at: DateTime<Utc>,
}

struct ResolvedPayment {
    payment_method_id: Uuid,
    wallet_id: Uuid,
    amount: Decimal,
}

async fn resolve_payments(
    conn: &mut sqlx::PgConnection,
    payments_input: &[PurchasePaymentInput],
    on_account: Decimal,
    total: Decimal,
) -> Result<Vec<ResolvedPayment>, RepoError> {
    if on_account.is_sign_negative() {
        return Err(RepoError::OnAccountNegative);
    }
    if on_account.round_dp(2) != on_account {
        return Err(RepoError::OnAccountPrecision);
    }
    if on_account > crate::money::MAX_AMOUNT {
        return Err(RepoError::OnAccountTooLarge);
    }

    let mut wallet_paid = Decimal::ZERO;
    let mut resolved = Vec::with_capacity(payments_input.len());

    for payment in payments_input {
        if payment.amount <= Decimal::ZERO {
            return Err(RepoError::PaymentAmountNotPositive);
        }
        if payment.amount.round_dp(2) != payment.amount {
            return Err(RepoError::PaymentAmountPrecision);
        }
        if payment.amount > crate::money::MAX_AMOUNT {
            return Err(RepoError::PaymentAmountTooLarge);
        }
        wallet_paid = crate::money::checked_add_amount(wallet_paid, payment.amount)
            .ok_or(RepoError::PaymentAmountTooLarge)?;

        let method = payments::find_method(&mut *conn, payment.payment_method_id)
            .await
            .map_err(map_payment_error)?
            .ok_or(RepoError::PaymentMethodUnavailable)?;

        if !method.is_active || !method.wallet_is_active {
            return Err(RepoError::PaymentMethodUnavailable);
        }

        resolved.push(ResolvedPayment {
            payment_method_id: method.id,
            wallet_id: method.wallet_id,
            amount: payment.amount,
        });
    }

    let paid = crate::money::checked_add_amount(wallet_paid, on_account)
        .ok_or(RepoError::PaymentAmountTooLarge)?;
    if paid != total {
        return Err(RepoError::PaymentMismatch {
            total,
            paid,
            on_account,
        });
    }

    Ok(resolved)
}

fn map_payment_error(err: payments::RepoError) -> RepoError {
    match err {
        payments::RepoError::Database(sql) => RepoError::Database(sql),
        _ => RepoError::PaymentMethodUnavailable,
    }
}

fn map_authorize_error(err: crate::supplier_accounts::repository::RepoError) -> RepoError {
    use crate::supplier_accounts::repository::RepoError as AccountErr;
    match err {
        AccountErr::SupplierNotFound => RepoError::SupplierNotFound,
        AccountErr::SupplierInactive => RepoError::SupplierInactive,
        AccountErr::CreditLimitExceeded { available, .. } => {
            RepoError::OnAccountExceedsCreditLimit { available }
        }
        AccountErr::SupplierBusy => RepoError::SupplierAccountBusy,
        AccountErr::Database(sql) => RepoError::Database(sql),
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "supplier accounts authorize_credit: {other}"
        ))),
    }
}

fn map_account_write_error(err: crate::supplier_accounts::repository::RepoError) -> RepoError {
    use crate::supplier_accounts::repository::RepoError as AccountErr;
    match err {
        AccountErr::SupplierBusy => RepoError::SupplierAccountBusy,
        AccountErr::Database(sql) => RepoError::Database(sql),
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "supplier accounts ledger write: {other}"
        ))),
    }
}

/// Lock the purchase row and confirm the draft can still be completed.
/// Returns the supplier the draft names, if any: completion requires one,
/// so the caller turns `None` into `SupplierRequired`.
async fn lock_draft(
    conn: &mut sqlx::PgConnection,
    purchase_id: Uuid,
) -> Result<Option<Uuid>, RepoError> {
    let purchase_row = sqlx::query!(
        r#"
        SELECT status AS "status: PurchaseStatus", supplier_id
        FROM purchases
        WHERE id = $1
        FOR UPDATE
        "#,
        purchase_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !purchase_row.status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    Ok(purchase_row.supplier_id)
}

/// A line of the draft as completion reads it: how much stock arrives, at
/// the cost frozen when the line was added.
struct DraftLine {
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
}

/// The draft's lines in product order, so the product locks below are
/// always acquired sorted by id.
async fn load_draft_lines(
    conn: &mut sqlx::PgConnection,
    purchase_id: Uuid,
) -> Result<Vec<DraftLine>, RepoError> {
    let items = sqlx::query!(
        r#"
        SELECT pi.product_id, pi.quantity, pi.unit_cost
        FROM purchase_items pi
        WHERE pi.purchase_id = $1
        ORDER BY pi.product_id
        "#,
        purchase_id
    )
    .fetch_all(&mut *conn)
    .await?;

    if items.is_empty() {
        return Err(RepoError::EmptyPurchase);
    }

    Ok(items
        .into_iter()
        .map(|item| DraftLine {
            product_id: item.product_id,
            quantity: item.quantity,
            unit_cost: item.unit_cost,
        })
        .collect())
}

/// Pass 1: lock each product, confirm still active and stock-tracked, compute total.
async fn lock_products_and_compute_total(
    conn: &mut sqlx::PgConnection,
    items: &[DraftLine],
) -> Result<Decimal, RepoError> {
    let mut total = Decimal::ZERO;
    for item in items {
        let product = sqlx::query!(
            r#"
            SELECT kind AS "kind: ProductKind", is_active
            FROM products
            WHERE id = $1
            FOR UPDATE
            "#,
            item.product_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(RepoError::ProductNotFound)?;

        if !product.is_active {
            return Err(RepoError::ProductInactive);
        }
        if !product.kind.tracks_stock() {
            return Err(RepoError::ProductDoesNotTrackStock);
        }

        let subtotal = crate::money::checked_line_subtotal(item.quantity, item.unit_cost)
            .ok_or(RepoError::TotalTooLarge)?;
        total =
            crate::money::checked_add_amount(total, subtotal).ok_or(RepoError::TotalTooLarge)?;
    }

    Ok(total)
}

/// Lock and validate the supplier and payments before any write.
///
/// On the cash path (`on_account == 0`), lock `suppliers FOR UPDATE` and
/// verify `is_active` before resolving payments so an inactive supplier
/// keeps its baseline `409 SupplierInactive` precedence over `422` payment
/// errors. On the on-account path (`on_account > 0`), validate payment
/// arithmetic and methods first so malformed sums do not acquire the
/// supplier advisory lock, then run `authorize_credit` (advisory lock
/// seed `1` -> `suppliers FOR UPDATE`). Both paths preserve the global lock
/// order `purchases` -> `products` -> [advisory lock] -> `suppliers`.
///
/// Returns the resolved charges and the supplier name to freeze on the
/// purchase.
async fn resolve_supplier_and_payments(
    conn: &mut sqlx::PgConnection,
    supplier_id: Uuid,
    payments_input: &[PurchasePaymentInput],
    on_account: Decimal,
    total: Decimal,
) -> Result<(Vec<ResolvedPayment>, String), RepoError> {
    let (charges, supplier_name) = if on_account > Decimal::ZERO {
        let charges = resolve_payments(&mut *conn, payments_input, on_account, total).await?;

        crate::supplier_accounts::repository::authorize_credit(&mut *conn, supplier_id, on_account)
            .await
            .map_err(map_authorize_error)?;

        let name = sqlx::query_scalar!("SELECT name FROM suppliers WHERE id = $1", supplier_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(RepoError::SupplierNotFound)?;

        (charges, name)
    } else {
        let supplier = sqlx::query!(
            "SELECT name, is_active FROM suppliers WHERE id = $1 FOR UPDATE",
            supplier_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(RepoError::SupplierNotFound)?;

        if !supplier.is_active {
            return Err(RepoError::SupplierInactive);
        }

        let charges = resolve_payments(&mut *conn, payments_input, on_account, total).await?;
        (charges, supplier.name)
    };

    Ok((charges, supplier_name))
}

/// Pass 2: record stock inflows.
async fn insert_stock_movements(
    conn: &mut sqlx::PgConnection,
    items: &[DraftLine],
    ledger_note: &str,
    actor: Uuid,
) -> Result<Vec<Uuid>, RepoError> {
    let mut movement_ids = Vec::with_capacity(items.len());
    for item in items {
        let movement_id = Uuid::now_v7();
        sqlx::query!(
            r#"
            INSERT INTO stock_movements (id, product_id, reason, quantity, unit_cost, note, created_by)
            VALUES ($1, $2, 'purchase', $3, $4, $5, $6)
            "#,
            movement_id,
            item.product_id,
            item.quantity,
            Some(item.unit_cost),
            Some(ledger_note),
            actor,
        )
        .execute(&mut *conn)
        .await?;

        movement_ids.push(movement_id);
    }

    Ok(movement_ids)
}

/// Pass 3: record wallet outflows for the upfront cash legs.
async fn insert_payment_movements(
    conn: &mut sqlx::PgConnection,
    charges: &[ResolvedPayment],
    purchase_id: Uuid,
    ledger_note: &str,
    actor: Uuid,
) -> Result<Vec<Uuid>, RepoError> {
    let mut payment_movement_ids = Vec::with_capacity(charges.len());
    for charge in charges {
        let movement_id = payments::insert_movement(
            &mut *conn,
            payments::NewWalletMovement {
                wallet_id: charge.wallet_id,
                kind: WalletMovementKind::PurchasePayment,
                amount: charge.amount,
                sale_id: None,
                purchase_id: Some(purchase_id),
                payment_method_id: Some(charge.payment_method_id),
                credit_note_id: None,
                purchase_return_id: None,
                customer_id: None,
                supplier_id: None,
                note: Some(ledger_note),
                created_by: Some(actor),
            },
        )
        .await
        .map_err(map_payment_error)?;

        payment_movement_ids.push(movement_id);
    }

    Ok(payment_movement_ids)
}

/// Close the purchase and freeze supplier_name + total in the same statement.
async fn close_purchase(
    conn: &mut sqlx::PgConnection,
    purchase_id: Uuid,
    total: Decimal,
    supplier_name: &str,
    actor: Uuid,
    note: Option<&str>,
) -> Result<DateTime<Utc>, RepoError> {
    let completed_at = sqlx::query_scalar!(
        r#"
        UPDATE purchases
        SET status        = 'completed',
            total         = $2,
            supplier_name = $3,
            completed_by  = $4,
            completed_at  = now(),
            note          = $5
        WHERE id = $1
        RETURNING completed_at AS "completed_at!"
        "#,
        purchase_id,
        total,
        supplier_name,
        actor,
        note,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(completed_at)
}

/// Record the on-account payable and automatically apply any available
/// unapplied supplier advance credit oldest-first inside the same commit.
///
/// Returns the credit applied, zero when nothing was on account.
async fn charge_supplier_account(
    conn: &mut sqlx::PgConnection,
    supplier_id: Uuid,
    purchase_id: Uuid,
    on_account: Decimal,
    actor: Uuid,
) -> Result<Decimal, RepoError> {
    let mut credit_applied = Decimal::ZERO;
    if on_account > Decimal::ZERO {
        crate::supplier_accounts::repository::record_charge(
            &mut *conn,
            supplier_id,
            purchase_id,
            on_account,
            actor,
        )
        .await
        .map_err(map_account_write_error)?;

        credit_applied = crate::supplier_accounts::repository::apply_available_credit(
            &mut *conn,
            supplier_id,
            purchase_id,
            on_account,
        )
        .await
        .map_err(map_account_write_error)?;
    }

    Ok(credit_applied)
}

/// Complete a draft purchase in one atomic transaction.
pub async fn complete(
    pool: &PgPool,
    purchase_id: Uuid,
    actor: Uuid,
    payments_input: &[PurchasePaymentInput],
    on_account: Decimal,
    note: Option<&str>,
) -> Result<CompletedPurchase, RepoError> {
    let mut tx = pool.begin().await?;

    let supplier_id = lock_draft(&mut tx, purchase_id)
        .await?
        .ok_or(RepoError::SupplierRequired)?;
    let items = load_draft_lines(&mut tx, purchase_id).await?;
    let total = lock_products_and_compute_total(&mut tx, &items).await?;
    let (charges, supplier_name) =
        resolve_supplier_and_payments(&mut tx, supplier_id, payments_input, on_account, total)
            .await?;

    let ledger_note = format!("Purchase {}", &purchase_id.to_string()[..8]);

    let movement_ids = insert_stock_movements(&mut tx, &items, &ledger_note, actor).await?;
    let payment_movement_ids =
        insert_payment_movements(&mut tx, &charges, purchase_id, &ledger_note, actor).await?;
    let completed_at =
        close_purchase(&mut tx, purchase_id, total, &supplier_name, actor, note).await?;
    let credit_applied =
        charge_supplier_account(&mut tx, supplier_id, purchase_id, on_account, actor).await?;

    tx.commit().await?;

    Ok(CompletedPurchase {
        purchase_id,
        total,
        movement_ids,
        payment_movement_ids,
        credit_applied,
        completed_at,
    })
}
