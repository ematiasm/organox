//! Sales persistence.
//!
//! The draft and its items are written in the same transaction that validates
//! stock, as in the stock ledger: separate reads and writes allow the same unit
//! to be sold twice.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use super::model::{Sale, SaleItem, SalePaymentInput, SaleStatus};
use crate::payments::model::WalletMovementKind;
use crate::payments::repository as payments;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("sale not found")]
    NotFound,

    #[error("product not found")]
    ProductNotFound,

    #[error("the product does not sell stock (it is a service)")]
    ProductDoesNotTrackStock,

    #[error("insufficient stock")]
    InsufficientStock {
        product_name: String,
        available: Decimal,
        requested: Decimal,
    },

    #[error("the sale is no longer editable")]
    NotEditable,

    #[error("the sale is empty")]
    EmptySale,

    #[error("the quantity has more than 3 decimal places")]
    QuantityPrecision,

    #[error("the quantity exceeds the allowed maximum")]
    QuantityTooLarge,

    #[error("the sale total exceeds the allowed maximum")]
    TotalTooLarge,

    #[error("the payment amount must be greater than zero")]
    PaymentAmountNotPositive,

    #[error("the payment amount has more than 2 decimal places")]
    PaymentAmountPrecision,

    #[error("the payment amount exceeds the allowed maximum")]
    PaymentAmountTooLarge,

    #[error("the payments do not add up to the sale total")]
    PaymentMismatch {
        total: Decimal,
        paid: Decimal,
        on_account: Decimal,
    },

    #[error("the on-account amount cannot be negative")]
    OnAccountNegative,

    #[error("the on-account amount has more than 2 decimal places")]
    OnAccountPrecision,

    #[error("the on-account amount exceeds the allowed maximum")]
    OnAccountTooLarge,

    #[error("an on-account amount needs a customer on the sale")]
    OnAccountNeedsCustomer,

    #[error("an on-account amount needs an active customer")]
    OnAccountNeedsActiveCustomer,

    #[error("the on-account amount exceeds the {available} the customer still has available")]
    OnAccountExceedsCreditLimit { available: Decimal },

    #[error("the customer account is busy, retry")]
    CustomerAccountBusy,

    #[error("the payment method is unknown or inactive")]
    PaymentMethodUnavailable,

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub struct NewSaleDraft {
    pub created_by: Uuid,
}

/// Create an empty draft.
pub async fn create_draft(pool: &PgPool, new: NewSaleDraft) -> Result<Uuid, RepoError> {
    let id = Uuid::now_v7();

    sqlx::query!(
        r#"
        INSERT INTO sales (id, status, created_by)
        VALUES ($1, 'draft', $2)
        "#,
        id,
        new.created_by,
    )
    .execute(pool)
    .await?;

    Ok(id)
}

/// Recalculate the draft total from its items.
///
/// Always recalculated on the server: the client's total is only for display.
/// Returning a browser-calculated total would trust the browser to charge.
/// Runs inside the caller's transaction (which already holds `FOR UPDATE` on
/// the `sales` row) and keeps `AND status = 'draft'` as defense-in-depth.
async fn refresh_total(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sale_id: Uuid,
) -> Result<Decimal, RepoError> {
    let total = sqlx::query_scalar!(
        r#"
        UPDATE sales
        SET total = COALESCE((
            SELECT SUM(ROUND(quantity * unit_price, 2)) FROM sale_items WHERE sale_id = $1
        ), 0)
        WHERE id = $1 AND status = 'draft'
        RETURNING COALESCE(total, 0) AS "total!"
        "#,
        sale_id
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RepoError::NotEditable)?;

    Ok(total)
}

/// Add a product to the draft.
///
/// Runs in a transaction that locks the `sales` row `FOR UPDATE` so completion
/// or cancellation cannot commit between the status check and the item write.
/// If already present, ADD the quantity. Uniqueness is guaranteed by the
/// `(sale_id, product_id)` index with `ON CONFLICT`, not a prior read: two
/// concurrent requests from the same cashier (double-clicking "add") must not
/// create two rows.
pub async fn upsert_item(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<(), RepoError> {
    let mut tx = pool.begin().await?;

    let sale_status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1 FOR UPDATE"#,
        sale_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !sale_status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    // Take the product price NOW and freeze it in the item.
    let product = sqlx::query!(
        r#"
        SELECT kind AS "kind: crate::catalog::model::ProductKind", name, price
        FROM products
        WHERE id = $1 AND is_active = true
        "#,
        product_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::ProductNotFound)?;

    if !product.kind.tracks_stock() {
        return Err(RepoError::ProductDoesNotTrackStock);
    }

    // A product without a price cannot be charged. Reject it here instead of
    // assuming 0, which is what the customer would otherwise be charged.
    let unit_price = product.price.ok_or(RepoError::ProductDoesNotTrackStock)?;

    if quantity.round_dp(3) != quantity {
        return Err(RepoError::QuantityPrecision);
    }
    if quantity > crate::money::MAX_QUANTITY {
        return Err(RepoError::QuantityTooLarge);
    }

    // Validate the summed line quantity (`numeric(12, 3)`) and projected sale
    // total (`numeric(14, 2)`) under the `sales FOR UPDATE` lock before writing,
    // turning Postgres `22003` numeric overflows into a typed 422 rejection.
    let existing_items = sqlx::query!(
        r#"
        SELECT product_id, quantity, unit_price
        FROM sale_items
        WHERE sale_id = $1
        "#,
        sale_id
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut found = false;
    let mut projected = Vec::with_capacity(existing_items.len() + 1);
    for item in existing_items {
        if item.product_id == product_id {
            let new_qty = crate::money::checked_add_quantity(item.quantity, quantity)
                .ok_or(RepoError::QuantityTooLarge)?;
            projected.push((new_qty, item.unit_price));
            found = true;
        } else {
            projected.push((item.quantity, item.unit_price));
        }
    }
    if !found {
        projected.push((quantity, unit_price));
    }
    crate::money::checked_document_total(projected).ok_or(RepoError::TotalTooLarge)?;

    sqlx::query!(
        r#"
        INSERT INTO sale_items (id, sale_id, product_id, quantity, unit_price)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (sale_id, product_id) DO UPDATE
        SET quantity = sale_items.quantity + EXCLUDED.quantity
        "#,
        Uuid::now_v7(),
        sale_id,
        product_id,
        quantity,
        unit_price,
    )
    .execute(&mut *tx)
    .await?;

    refresh_total(&mut tx, sale_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Set an item's quantity. Quantity 0 removes it.
///
/// Locks the `sales` row `FOR UPDATE` in a transaction so completion or
/// cancellation cannot race between the draft check, item mutation, and total
/// recalculation.
pub async fn set_item_quantity(
    pool: &PgPool,
    sale_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
) -> Result<(), RepoError> {
    if quantity > Decimal::ZERO {
        if quantity.round_dp(3) != quantity {
            return Err(RepoError::QuantityPrecision);
        }
        if quantity > crate::money::MAX_QUANTITY {
            return Err(RepoError::QuantityTooLarge);
        }
    }

    let mut tx = pool.begin().await?;

    let sale_status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1 FOR UPDATE"#,
        sale_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !sale_status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    if quantity <= Decimal::ZERO {
        sqlx::query!(
            "DELETE FROM sale_items WHERE sale_id = $1 AND product_id = $2",
            sale_id,
            product_id
        )
        .execute(&mut *tx)
        .await?;
    } else {
        let existing_items = sqlx::query!(
            r#"
            SELECT product_id, quantity, unit_price
            FROM sale_items
            WHERE sale_id = $1
            "#,
            sale_id
        )
        .fetch_all(&mut *tx)
        .await?;

        let mut found = false;
        let mut projected = Vec::with_capacity(existing_items.len());
        for item in existing_items {
            if item.product_id == product_id {
                projected.push((quantity, item.unit_price));
                found = true;
            } else {
                projected.push((item.quantity, item.unit_price));
            }
        }
        if !found {
            return Err(RepoError::ProductNotFound);
        }
        crate::money::checked_document_total(projected).ok_or(RepoError::TotalTooLarge)?;

        sqlx::query!(
            "UPDATE sale_items SET quantity = $3 WHERE sale_id = $1 AND product_id = $2",
            sale_id,
            product_id,
            quantity
        )
        .execute(&mut *tx)
        .await?;
    }

    refresh_total(&mut tx, sale_id).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn remove_item(pool: &PgPool, sale_id: Uuid, product_id: Uuid) -> Result<(), RepoError> {
    set_item_quantity(pool, sale_id, product_id, Decimal::ZERO).await
}

/// Attach or clear the sale's customer. Draft-only: same rule as every other
/// draft mutation, checked the same way.
///
/// Existence and active state are NOT checked here: they are the customers
/// module's rule (`service::get_customer`), and a copy in sales SQL would drift.
pub async fn set_customer(
    pool: &PgPool,
    sale_id: Uuid,
    customer_id: Option<Uuid>,
) -> Result<(), RepoError> {
    // One statement instead of "read the status, then write": the sale could be
    // completed in between, and a customer attached after completion would land
    // on a closed sale with no snapshot — a sale whose name could still change.
    // The draft guard therefore lives in the WHERE clause. The item mutations
    // above lock the row in a transaction because they span multiple tables;
    // this one does not.
    let result = sqlx::query!(
        "UPDATE sales SET customer_id = $2 WHERE id = $1 AND status = 'draft'",
        sale_id,
        customer_id,
    )
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        return Ok(());
    }

    // Nothing was updated: report why. A frame that matched no row is either a
    // missing sale or a closed one, never a draft.
    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: SaleStatus" FROM sales WHERE id = $1"#,
        sale_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::NotFound)?;

    if status.is_editable() {
        return Err(RepoError::Database(sqlx::Error::Protocol(
            "the draft sale was not updated".to_string(),
        )));
    }

    Err(RepoError::NotEditable)
}

/// Cancel a draft. Does not touch the stock ledger because a draft never did:
/// that is the virtue of not deducting until checkout.
pub async fn cancel(pool: &PgPool, sale_id: Uuid) -> Result<(), RepoError> {
    let result = sqlx::query!(
        "UPDATE sales SET status = 'cancelled' WHERE id = $1 AND status = 'draft'",
        sale_id
    )
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // Either it does not exist, or it was no longer a draft.
        let exists = sqlx::query_scalar!("SELECT 1::bool FROM sales WHERE id = $1", sale_id)
            .fetch_optional(pool)
            .await?;
        return Err(if exists.is_some() {
            RepoError::NotEditable
        } else {
            RepoError::NotFound
        });
    }

    Ok(())
}

pub async fn find_sale(pool: &PgPool, sale_id: Uuid) -> Result<Option<Sale>, RepoError> {
    let row = sqlx::query!(
        r#"
        SELECT s.id, s.status AS "status: SaleStatus", s.total, s.created_by, s.created_at, s.completed_at,
               -- Name precedence: a completed sale carries the frozen snapshot
               -- (non-NULL), which wins; a draft has none yet, so the joined
               -- live name shows and still tracks a rename.
               COALESCE(s.customer_name, c.name) AS "customer_name?",
               s.customer_id
        FROM sales s
        LEFT JOIN customers c ON c.id = s.customer_id
        WHERE s.id = $1
        "#,
        sale_id
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let items = sqlx::query!(
        r#"
        SELECT si.id, si.product_id, p.name AS "product_name!", si.quantity, si.unit_price
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = $1
        ORDER BY si.created_at, si.id
        "#,
        sale_id
    )
    .fetch_all(pool)
    .await?;

    let frozen_total = if row.status == SaleStatus::Completed {
        row.total
    } else {
        None
    };

    Ok(Some(Sale {
        id: row.id,
        status: row.status,
        frozen_total,
        created_by: row.created_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
        customer_id: row.customer_id,
        customer_name: row.customer_name,
        items: items
            .into_iter()
            .map(|i| SaleItem {
                id: i.id,
                product_id: i.product_id,
                product_name: i.product_name,
                quantity: i.quantity,
                unit_price: i.unit_price,
            })
            .collect(),
    }))
}

/// A user's open draft, if any.
///
/// A cashier has **one** draft at a time: opening another would leave two live
/// carts and stock reserved twice in the cashier's mind.
pub async fn find_open_draft_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<Uuid>, RepoError> {
    let id = sqlx::query_scalar!(
        r#"
        SELECT id FROM sales
        WHERE status = 'draft' AND created_by = $1
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await?;

    Ok(id)
}

#[derive(Debug)]
pub struct CompletedSale {
    pub sale_id: Uuid,
    pub total: Decimal,
    /// How much of the on-account part the customer's available credit paid
    /// at completion. `on_account - credit_applied` is the debt the sale
    /// actually added.
    pub credit_applied: Decimal,
    pub movement_ids: Vec<Uuid>,
    pub payment_movement_ids: Vec<Uuid>,
    pub completed_at: DateTime<Utc>,
}

/// One payment resolved against its method: which wallet it pays into, and the
/// amount already rounded to the column's scale.
struct ResolvedCharge {
    payment_method_id: Uuid,
    wallet_id: Uuid,
    amount: Decimal,
}

/// Check the payments against the total and resolve each one to its wallet.
///
/// Runs inside the sale transaction on purpose: the method and its wallet are
/// read from the same snapshot the charge uses, so a re-pointing committed
/// mid-charge cannot split the money from the total.
async fn resolve_payments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payments_input: &[SalePaymentInput],
    on_account: Decimal,
    total: Decimal,
) -> Result<Vec<ResolvedCharge>, RepoError> {
    // No empty-list rejection here on purpose: a zero-total sale (a sample, a
    // warranty replacement) has no money to route, and the sum check below
    // already rejects an empty list whenever there IS something to collect
    // (at the till or on account).
    let mut paid = Decimal::ZERO;
    let mut resolved = Vec::with_capacity(payments_input.len());

    for payment in payments_input {
        if payment.amount <= Decimal::ZERO {
            return Err(RepoError::PaymentAmountNotPositive);
        }
        // Rejected, not silently rounded: a client that sends 100.005 is asking
        // for something the column cannot hold, and charging it as 100.00 would
        // quietly change the request. Same rule the catalog applies to prices.
        if payment.amount.round_dp(2) != payment.amount {
            return Err(RepoError::PaymentAmountPrecision);
        }
        if payment.amount > crate::money::MAX_AMOUNT {
            return Err(RepoError::PaymentAmountTooLarge);
        }
        paid = crate::money::checked_add_amount(paid, payment.amount)
            .ok_or(RepoError::PaymentAmountTooLarge)?;
        let amount = payment.amount;

        let method = payments::find_method(&mut **tx, payment.payment_method_id)
            .await
            .map_err(map_payment_error)?
            .ok_or(RepoError::PaymentMethodUnavailable)?;

        // A deactivated method or a deactivated wallet accepts no new money.
        if !method.is_active || !method.wallet_is_active {
            return Err(RepoError::PaymentMethodUnavailable);
        }

        resolved.push(ResolvedCharge {
            payment_method_id: method.id,
            wallet_id: method.wallet_id,
            amount,
        });
    }

    // The completion invariant: what the wallets collect plus what the
    // account takes must equal the total the server computed, exactly.
    // `paid` carries both parts together, so the rejection names the amount
    // the request actually covered; `on_account` travels separately because
    // only a request that used the account may mention it in the message.
    let covered = crate::money::checked_add_amount(paid, on_account)
        .ok_or(RepoError::PaymentAmountTooLarge)?;
    if covered != total {
        return Err(RepoError::PaymentMismatch {
            total,
            paid: covered,
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

/// `authorize_credit` re-runs, under the customer's lock, what this
/// transaction is about to act on. Its typed refusals keep the answers the
/// completion already gives: an inactive customer stays the 422 it has
/// always been, and a limit rejection joins the stock conflict as a 409 that
/// names what is left. The busy lock and database errors pass through.
fn map_authorize_error(err: crate::customer_accounts::repository::RepoError) -> RepoError {
    use crate::customer_accounts::repository::RepoError as AuthorizeError;
    match err {
        AuthorizeError::Database(sql) => RepoError::Database(sql),
        AuthorizeError::CustomerBusy => RepoError::CustomerAccountBusy,
        AuthorizeError::CustomerInactive => RepoError::OnAccountNeedsActiveCustomer,
        AuthorizeError::CreditLimitExceeded { available, .. } => {
            RepoError::OnAccountExceedsCreditLimit { available }
        }
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "authorize_credit refused a charge the completion had validated: {other}"
        ))),
    }
}

/// `record_charge` re-validates what this transaction already proved — the
/// sale is completed and belongs to the customer, the amount is cents, and
/// the ceiling it re-derives is the uncollected remainder — so its typed
/// refusals name conditions that cannot hold here and surface as internal
/// errors rather than a misleading typed rejection. The busy lock is the one
/// reachable refusal: a collection holding the customer's account.
fn map_charge_error(err: crate::customer_accounts::repository::RepoError) -> RepoError {
    use crate::customer_accounts::repository::RepoError as ChargeError;
    match err {
        ChargeError::Database(sql) => RepoError::Database(sql),
        ChargeError::CustomerBusy => RepoError::CustomerAccountBusy,
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "record_charge refused a charge the completion had validated: {other}"
        ))),
    }
}

/// `apply_available_credit` spends money this transaction already proved the
/// account holds, on the sale it just charged, within caps it re-derives from
/// the same rows: a guard firing inside it means those caps disagree, which is
/// a bug, so it surfaces as an internal error and the whole completion dies
/// rather than committing a charge its own payment could not cover. The busy
/// lock is the one reachable refusal, inherited from the charge above.
fn map_apply_error(err: crate::customer_accounts::repository::RepoError) -> RepoError {
    use crate::customer_accounts::repository::RepoError as ApplyError;
    match err {
        ApplyError::Database(sql) => RepoError::Database(sql),
        ApplyError::CustomerBusy => RepoError::CustomerAccountBusy,
        // The caps inside `apply_available_credit` re-derived everything the
        // ledger's guards check, so this firing is an invariant violation:
        // internal, never a 4xx the operator could act on.
        ApplyError::CreditApplicationBroken => RepoError::Database(sqlx::Error::Protocol(
            "the credit application disagreed with the ledger's own guards".into(),
        )),
        other => RepoError::Database(sqlx::Error::Protocol(format!(
            "apply_available_credit refused an allocation the completion had validated: {other}"
        ))),
    }
}

/// Complete the sale: validate stock, write movements, charge the account
/// part and close it.
///
/// All in ONE transaction with `SELECT ... FOR UPDATE` per product. This is the
/// same discipline as `record_movement`: otherwise two registers can sell the
/// last unit at the same time.
///
/// `on_account` is the part of the total charged to the customer's account
/// instead of the wallets: zero for an ordinary cash sale, at most the total,
/// and written as one `sale_charge` in the customer's ledger inside this same
/// transaction.
pub async fn complete(
    pool: &PgPool,
    sale_id: Uuid,
    actor: Uuid,
    payments_input: &[SalePaymentInput],
    on_account: Decimal,
) -> Result<CompletedSale, RepoError> {
    // The account takes debt, it does not give change: a negative amount is a
    // request to mint credit, not a payment. And a third decimal is something
    // `numeric(14,2)` cannot hold — rejected, never rounded, the same rule the
    // payments follow. Checked before anything is read: neither depends on
    // the sale, and rejecting here leaves nothing to roll back.
    if on_account < Decimal::ZERO {
        return Err(RepoError::OnAccountNegative);
    }
    if on_account.round_dp(2) != on_account {
        return Err(RepoError::OnAccountPrecision);
    }
    if on_account > crate::money::MAX_AMOUNT {
        return Err(RepoError::OnAccountTooLarge);
    }

    let mut tx = pool.begin().await?;

    let sale_row = sqlx::query!(
        r#"SELECT status AS "status: SaleStatus", customer_id
           FROM sales WHERE id = $1 FOR UPDATE"#,
        sale_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RepoError::NotFound)?;

    if !sale_row.status.is_editable() {
        return Err(RepoError::NotEditable);
    }

    let items = sqlx::query!(
        r#"
        SELECT si.product_id, si.quantity, si.unit_price, p.name
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = $1
        ORDER BY si.product_id
        "#,
        sale_id
    )
    .fetch_all(&mut *tx)
    .await?;

    if items.is_empty() {
        return Err(RepoError::EmptySale);
    }

    // Pass 1: lock each product, check stock and compute the total. Nothing is
    // written yet, so a rejected payment leaves no trace at all.
    let mut total = Decimal::ZERO;

    for item in &items {
        // Lock the product row: serialize concurrent outflows for this product,
        // as in the stock ledger.
        // `query_scalar!` because `query!` with a SELECT returning rows
        // requires `fetch_*`, not `execute`.
        sqlx::query_scalar!(
            "SELECT id FROM products WHERE id = $1 FOR UPDATE",
            item.product_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let available: Decimal = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(SUM(
                CASE reason
                    WHEN 'purchase'        THEN quantity
                    WHEN 'return'          THEN quantity
                    WHEN 'adjustment'      THEN quantity
                    WHEN 'sale'            THEN -quantity
                    WHEN 'purchase_return' THEN -quantity
                    WHEN 'loss'            THEN -quantity
                END
            ), 0) AS "stock!"
            FROM stock_movements
            WHERE product_id = $1
            "#,
            item.product_id
        )
        .fetch_one(&mut *tx)
        .await?;

        if available < item.quantity {
            return Err(RepoError::InsufficientStock {
                product_name: item.name.clone(),
                available,
                requested: item.quantity,
            });
        }

        let subtotal = crate::money::checked_line_subtotal(item.quantity, item.unit_price)
            .ok_or(RepoError::TotalTooLarge)?;
        total =
            crate::money::checked_add_amount(total, subtotal).ok_or(RepoError::TotalTooLarge)?;
    }

    // The money must add up to the total the server computed — the wallets'
    // part plus the account's part — and every method must still be usable.
    // Checked before any write.
    let charges = resolve_payments(&mut tx, payments_input, on_account, total).await?;

    // An on-account part is debt, and debt is extended only within what the
    // registry still vouches for: the draft must carry a customer, that
    // customer must be active, and the exposure the sale would leave must fit
    // the customer's credit limit. One gate, `authorize_credit`, decides all
    // three under the customer's lock, before any write, so a rejection
    // leaves no trace and a deactivation racing the checkout cannot slip
    // between the gate and the charge. The gate runs ONLY when something is
    // on account: a customer deactivated mid-draft must not block a sale the
    // till collects in full.
    let charge_customer = if on_account > Decimal::ZERO {
        let customer_id = sale_row
            .customer_id
            .ok_or(RepoError::OnAccountNeedsCustomer)?;

        crate::customer_accounts::repository::authorize_credit(&mut tx, customer_id, on_account)
            .await
            .map_err(map_authorize_error)?;

        Some(customer_id)
    } else {
        None
    };

    let note = format!("Sale {}", &sale_id.to_string()[..8]);

    // Pass 2: the stock movements. This ledger is the ONLY thing that deducts:
    // the `sales` table has no stock column.
    let mut movement_ids = Vec::with_capacity(items.len());
    for item in &items {
        // The stock movement is the ONLY thing that deducts: the `sales` table
        // has no stock column, and the ledger remains the source of truth.
        let movement_id = Uuid::now_v7();
        sqlx::query!(
            r#"
            INSERT INTO stock_movements (id, product_id, reason, quantity, unit_cost, note, created_by)
            VALUES ($1, $2, 'sale', $3, $4, $5, $6)
            "#,
            movement_id,
            item.product_id,
            item.quantity,
            item.unit_price,
            Some(note.as_str()),
            actor,
        )
        .execute(&mut *tx)
        .await?;

        movement_ids.push(movement_id);
    }

    // Money movements: same transaction as the stock, so a charged sale either
    // records both sides or neither.
    let mut payment_movement_ids = Vec::with_capacity(charges.len());
    for charge in &charges {
        let movement_id = payments::insert_movement(
            &mut *tx,
            payments::NewWalletMovement {
                wallet_id: charge.wallet_id,
                kind: WalletMovementKind::SalePayment,
                amount: charge.amount,
                sale_id: Some(sale_id),
                purchase_id: None,
                payment_method_id: Some(charge.payment_method_id),
                credit_note_id: None,
                purchase_return_id: None,
                // Till money is never an account payment: the customer link
                // belongs to the collection endpoint alone.
                customer_id: None,
                supplier_id: None,
                note: Some(note.as_str()),
                created_by: Some(actor),
            },
        )
        .await
        .map_err(map_payment_error)?;

        payment_movement_ids.push(movement_id);
    }

    let completed_at = sqlx::query_scalar!(
        r#"
        UPDATE sales
        SET status = 'completed', total = $2, completed_by = $3, completed_at = now(),
            -- Freeze the customer name in the SAME statement that closes the
            -- sale: the snapshot and the status flip commit together, so a
            -- rename racing the checkout cannot slip in between. A sale
            -- without a customer stays NULL (the subquery matches no row), and
            -- a customer deactivated after the attach does NOT block the sale:
            -- the goods already left, and the snapshot is still taken.
            customer_name = (SELECT name FROM customers WHERE id = sales.customer_id)
        WHERE id = $1
        RETURNING completed_at AS "completed_at!"
        "#,
        sale_id,
        total,
        actor,
    )
    .fetch_one(&mut *tx)
    .await?;

    // The account's charge belongs to the same commit as the stock, the
    // wallets and the status flip: a sale and the debt it creates are written
    // together or not at all. `record_charge` re-derives its own ceiling — the
    // total minus what the wallets just collected — from the rows this
    // transaction wrote, so the charge is by construction exactly the
    // on-account part. Its advisory lock serializes this write against
    // collections on the same account.
    //
    // And the moment the charge exists, the customer's available credit pays
    // it: the same transaction allocates from their receipts, oldest first,
    // up to the on-account part. Automatic, not a flag — a customer holding
    // money the shop already collected must not be asked for cash again for
    // the same goods — and inside the same commit, so the debt and the credit
    // that covers it never appear as two states an operator could see apart.
    let mut credit_applied = Decimal::ZERO;
    if let Some(customer_id) = charge_customer {
        crate::customer_accounts::repository::record_charge(
            &mut tx,
            customer_id,
            sale_id,
            on_account,
            actor,
        )
        .await
        .map_err(map_charge_error)?;

        credit_applied = crate::customer_accounts::repository::apply_available_credit(
            &mut tx,
            customer_id,
            sale_id,
            on_account,
        )
        .await
        .map_err(map_apply_error)?;
    }

    tx.commit().await?;

    Ok(CompletedSale {
        sale_id,
        total,
        credit_applied,
        movement_ids,
        payment_movement_ids,
        completed_at,
    })
}

/// Completed sales, most recent first, plus the total number of completed
/// sales.
///
/// Items are fetched for every sale on the page in one `ANY($1)` query and
/// re-assembled in memory: a per-row `find_sale` would cost `1 + 2N` queries.
/// Completed sales and their lines are immutable, so the two statements stay
/// consistent without holding a snapshot transaction.
pub async fn list_completed(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Sale>, i64), RepoError> {
    let rows = sqlx::query!(
        r#"
        SELECT s.id, s.status AS "status: SaleStatus", s.total, s.created_by,
               s.created_at, s.completed_at,
               COALESCE(s.customer_name, c.name) AS "customer_name?",
               s.customer_id,
               COUNT(*) OVER () AS "row_total!"
        FROM sales s
        LEFT JOIN customers c ON c.id = s.customer_id
        WHERE s.status = 'completed'
        ORDER BY s.completed_at DESC, s.id DESC
        LIMIT $1 OFFSET $2
        "#,
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
        SELECT si.id, si.sale_id, si.product_id, p.name AS "product_name!",
               si.quantity, si.unit_price
        FROM sale_items si
        JOIN products p ON p.id = si.product_id
        WHERE si.sale_id = ANY($1)
        ORDER BY si.created_at, si.id
        "#,
        &ids
    )
    .fetch_all(pool)
    .await?;

    let mut items_by_sale: HashMap<Uuid, Vec<SaleItem>> = HashMap::new();
    for item in item_rows {
        items_by_sale
            .entry(item.sale_id)
            .or_default()
            .push(SaleItem {
                id: item.id,
                product_id: item.product_id,
                product_name: item.product_name,
                quantity: item.quantity,
                unit_price: item.unit_price,
            });
    }

    let sales = rows
        .into_iter()
        .map(|row| Sale {
            id: row.id,
            status: row.status,
            items: items_by_sale.remove(&row.id).unwrap_or_default(),
            frozen_total: if row.status == SaleStatus::Completed {
                row.total
            } else {
                None
            },
            created_by: row.created_by,
            created_at: row.created_at,
            completed_at: row.completed_at,
            customer_id: row.customer_id,
            customer_name: row.customer_name,
        })
        .collect();

    Ok((sales, total))
}

/// Total number of completed sales.
///
/// `list_completed` reports the count in each row, so this only runs when the
/// requested page is past the end and the window query returns nothing.
pub async fn count_completed(pool: &PgPool) -> Result<i64, RepoError> {
    let count =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM sales WHERE status = 'completed'"#)
            .fetch_one(pool)
            .await?;

    Ok(count)
}
