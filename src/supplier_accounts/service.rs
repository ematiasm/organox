//! Supplier account use cases: payments, credit refunds, manual adjustments,
//! account statements, logs, and payables aging reports.
//!
//! Mirrors [`crate::customer_accounts::service`]: one transaction, one lock
//! order — the supplier advisory lock (`seed = 1`) first, replay lookup under
//! that lock before anything is written, then method validation, wallet
//! movement, and ledger entry. A rejection anywhere rolls the whole transaction
//! back.

use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::payments::model::WalletMovementKind;
use crate::payments::repository as wallets;
use crate::supplier_accounts::model::{
    AgingBuckets, PayablesSummary, SupplierAccountEntryKind, SupplierAccountStatement,
    SupplierAccountStatementEntry, SupplierAdjustmentLogItem, SupplierPaymentLogItem,
};
use crate::supplier_accounts::repository::{self, RepoError};

#[derive(Debug, thiserror::Error)]
pub enum SupplierAccountError {
    #[error("the payment method is unknown or inactive")]
    MethodUnavailable,

    #[error("supplier not found")]
    SupplierNotFound,

    #[error("purchase not found")]
    PurchaseNotFound,

    #[error("the supplier account is busy, retry")]
    SupplierBusy,

    #[error("that document is already recorded in the ledger")]
    AlreadyRecorded,

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("a credit refund needs a reason")]
    RefundNoteRequired,

    /// Typed rejections from the payables ledger with actionable messages.
    #[error("{0}")]
    Validation(RepoError),

    #[error("internal: {0}")]
    Internal(RepoError),
}

impl From<RepoError> for SupplierAccountError {
    fn from(err: RepoError) -> Self {
        match err {
            RepoError::SupplierNotFound => SupplierAccountError::SupplierNotFound,
            RepoError::PurchaseNotFound => SupplierAccountError::PurchaseNotFound,
            RepoError::SupplierBusy => SupplierAccountError::SupplierBusy,
            RepoError::AlreadyRecorded => SupplierAccountError::AlreadyRecorded,
            RepoError::AmountTooLarge { max } => SupplierAccountError::AmountTooLarge { max },
            RepoError::NonPositiveAmount
            | RepoError::TooManyDecimals { .. }
            | RepoError::PurchaseOfAnotherSupplier
            | RepoError::PurchaseNotChargeable
            | RepoError::OutstandingExceeded { .. }
            | RepoError::PaymentOverAllocated { .. }
            | RepoError::DuplicateAllocation { .. }
            | RepoError::MovementOfAnotherSupplier
            | RepoError::CreditRefundExceedsCredit { .. }
            | RepoError::AdjustmentCrossesZero { .. }
            | RepoError::AdjustmentNoteRequired
            | RepoError::NotAnAdjustment => SupplierAccountError::Validation(err),
            other => SupplierAccountError::Internal(other),
        }
    }
}

impl From<sqlx::Error> for SupplierAccountError {
    fn from(err: sqlx::Error) -> Self {
        SupplierAccountError::Internal(RepoError::Database(err))
    }
}

impl From<wallets::RepoError> for SupplierAccountError {
    fn from(err: wallets::RepoError) -> Self {
        match err {
            wallets::RepoError::Database(err) => {
                SupplierAccountError::Internal(RepoError::Database(err))
            }
            other => SupplierAccountError::Internal(RepoError::Database(sqlx::Error::Protocol(
                format!("payments repository: {other}"),
            ))),
        }
    }
}

/// Exact capacity of `numeric(14, 2)`: 12 integer digits and 2 decimals.
fn max_amount() -> Decimal {
    crate::money::MAX_AMOUNT
}

/// Boundary money validation in precedence order: sign, precision, magnitude.
fn validate_amount(amount: Decimal) -> Result<(), SupplierAccountError> {
    if amount <= Decimal::ZERO {
        return Err(SupplierAccountError::Validation(
            RepoError::NonPositiveAmount,
        ));
    }
    if amount.round_dp(2) != amount {
        return Err(SupplierAccountError::Validation(
            RepoError::TooManyDecimals { amount },
        ));
    }
    if amount > max_amount() {
        return Err(SupplierAccountError::AmountTooLarge { max: max_amount() });
    }
    Ok(())
}

// --- Supplier payments --------------------------------------------------

/// One payment against a supplier's account. Allocations are explicit and
/// never invented by the server; any unallocated remainder becomes unapplied
/// advance credit.
pub struct RecordPaymentInput<'a> {
    pub payment_method_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub actor: Uuid,
}

/// The recorded (or replayed) supplier payment together with the account
/// position and remaining outstanding balance on each allocated purchase.
pub struct RecordedPayment {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
    /// Each allocated purchase and its outstanding payable after this payment.
    pub allocations: Vec<(Uuid, Decimal)>,
    pub replayed: bool,
}

async fn position_with_outstanding(
    conn: &mut PgConnection,
    supplier_id: Uuid,
    purchase_ids: impl Iterator<Item = Uuid>,
) -> Result<(Decimal, Decimal, Vec<(Uuid, Decimal)>), RepoError> {
    let position = repository::account_position_in(conn, supplier_id).await?;
    let mut outstanding = Vec::new();
    for purchase_id in purchase_ids {
        let owed = repository::outstanding_for_purchase_in(conn, purchase_id).await?;
        outstanding.push((purchase_id, owed));
    }
    Ok((position.balance, position.unapplied_credit, outstanding))
}

/// Pay money against a supplier's account (`supplier_account_payment` wallet
/// outflow + `payment` ledger entry + explicit allocations) in one transaction.
pub async fn record_payment(
    pool: &PgPool,
    supplier_id: Uuid,
    input: RecordPaymentInput<'_>,
) -> Result<RecordedPayment, SupplierAccountError> {
    validate_amount(input.amount)?;

    let mut tx = pool.begin().await?;
    repository::lock_supplier(&mut tx, supplier_id).await?;

    if let Some(key) = input.idempotency_key
        && let Some(existing) = repository::payment_by_key_in(&mut tx, supplier_id, key).await?
    {
        let (balance, unapplied_credit, allocations) =
            position_with_outstanding(&mut tx, supplier_id, existing.purchase_ids.iter().copied())
                .await?;
        tx.commit().await?;

        return Ok(RecordedPayment {
            entry_id: existing.entry_id,
            wallet_movement_id: existing.wallet_movement_id,
            balance,
            unapplied_credit,
            allocations,
            replayed: true,
        });
    }

    let method = wallets::find_method(&mut *tx, input.payment_method_id)
        .await?
        .ok_or(SupplierAccountError::MethodUnavailable)?;
    if !method.is_active || !method.wallet_is_active {
        return Err(SupplierAccountError::MethodUnavailable);
    }

    let movement_id = wallets::insert_movement(
        &mut *tx,
        wallets::NewWalletMovement {
            wallet_id: method.wallet_id,
            kind: WalletMovementKind::SupplierAccountPayment,
            amount: input.amount,
            sale_id: None,
            purchase_id: None,
            payment_method_id: Some(method.id),
            credit_note_id: None,
            purchase_return_id: None,
            customer_id: None,
            supplier_id: Some(supplier_id),
            note: input.note,
            created_by: Some(input.actor),
        },
    )
    .await?;

    let recorded = repository::record_payment(
        &mut tx,
        repository::NewSupplierPayment {
            supplier_id,
            wallet_movement_id: movement_id,
            amount: input.amount,
            allocations: input.allocations,
            idempotency_key: input.idempotency_key,
            note: input.note,
            created_by: input.actor,
        },
    )
    .await?;

    let (balance, unapplied_credit, allocations) = position_with_outstanding(
        &mut tx,
        supplier_id,
        input
            .allocations
            .iter()
            .map(|(purchase_id, _)| *purchase_id),
    )
    .await?;

    tx.commit().await?;

    Ok(RecordedPayment {
        entry_id: recorded.entry_id,
        wallet_movement_id: movement_id,
        balance,
        unapplied_credit,
        allocations,
        replayed: recorded.replayed,
    })
}

// --- Credit refunds -----------------------------------------------------

pub struct RefundCreditInput<'a> {
    pub payment_method_id: Uuid,
    pub amount: Decimal,
    pub note: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub actor: Uuid,
}

pub struct RefundedCredit {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
    pub replayed: bool,
}

/// Receive refunded advance credit back from a supplier through a
/// `supplier_account_refund` wallet inflow and a `credit_refund` ledger entry,
/// in one transaction.
pub async fn refund_credit(
    pool: &PgPool,
    supplier_id: Uuid,
    input: RefundCreditInput<'_>,
) -> Result<RefundedCredit, SupplierAccountError> {
    validate_amount(input.amount)?;
    let note = input.note.trim();
    if note.is_empty() {
        return Err(SupplierAccountError::RefundNoteRequired);
    }

    let mut tx = pool.begin().await?;
    repository::lock_supplier(&mut tx, supplier_id).await?;

    if let Some(key) = input.idempotency_key
        && let Some(existing) =
            repository::credit_refund_by_key_in(&mut tx, supplier_id, key).await?
    {
        let position = repository::account_position_in(&mut tx, supplier_id).await?;
        tx.commit().await?;
        return Ok(RefundedCredit {
            entry_id: existing.entry_id,
            wallet_movement_id: existing.wallet_movement_id,
            balance: position.balance,
            unapplied_credit: position.unapplied_credit,
            replayed: true,
        });
    }

    let method = wallets::find_method(&mut *tx, input.payment_method_id)
        .await?
        .ok_or(SupplierAccountError::MethodUnavailable)?;
    if !method.is_active || !method.wallet_is_active {
        return Err(SupplierAccountError::MethodUnavailable);
    }

    let movement_id = wallets::insert_movement(
        &mut *tx,
        wallets::NewWalletMovement {
            wallet_id: method.wallet_id,
            kind: WalletMovementKind::SupplierAccountRefund,
            amount: input.amount,
            sale_id: None,
            purchase_id: None,
            payment_method_id: Some(method.id),
            credit_note_id: None,
            purchase_return_id: None,
            customer_id: None,
            supplier_id: Some(supplier_id),
            note: Some(note),
            created_by: Some(input.actor),
        },
    )
    .await?;

    let entry_id = repository::record_credit_refund(
        &mut tx,
        repository::NewSupplierCreditRefund {
            supplier_id,
            wallet_movement_id: movement_id,
            amount: input.amount,
            idempotency_key: input.idempotency_key,
            note: Some(note),
            created_by: input.actor,
        },
    )
    .await?;

    let position = repository::account_position_in(&mut tx, supplier_id).await?;
    tx.commit().await?;

    Ok(RefundedCredit {
        entry_id,
        wallet_movement_id: movement_id,
        balance: position.balance,
        unapplied_credit: position.unapplied_credit,
        replayed: false,
    })
}

// --- Manual adjustments -------------------------------------------------

pub struct AdjustInput<'a> {
    pub kind: SupplierAccountEntryKind,
    pub amount: Decimal,
    pub note: &'a str,
    pub actor: Uuid,
}

pub struct RecordedAdjustment {
    pub entry_id: Uuid,
    pub kind: SupplierAccountEntryKind,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
}

/// Record a manual `adjustment_in` or `adjustment_out` without touching any
/// wallet. The ledger enforces that the balance stays non-negative.
pub async fn adjust(
    pool: &PgPool,
    supplier_id: Uuid,
    input: AdjustInput<'_>,
) -> Result<RecordedAdjustment, SupplierAccountError> {
    let note = input.note.trim();
    if note.is_empty() {
        return Err(SupplierAccountError::Validation(
            RepoError::AdjustmentNoteRequired,
        ));
    }
    validate_amount(input.amount)?;

    let mut tx = pool.begin().await?;
    let entry_id = repository::record_adjustment(
        &mut tx,
        supplier_id,
        input.kind,
        input.amount,
        note,
        input.actor,
    )
    .await?;
    let position = repository::account_position_in(&mut tx, supplier_id).await?;
    tx.commit().await?;

    Ok(RecordedAdjustment {
        entry_id,
        kind: input.kind,
        balance: position.balance,
        unapplied_credit: position.unapplied_credit,
    })
}

// --- Read use cases -----------------------------------------------------

/// Open a read-only repeatable-read transaction so multi-query read surfaces
/// observe one consistent ledger snapshot without blocking concurrent writers.
async fn begin_read_snapshot(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, SupplierAccountError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Complete statement for one supplier: ledger entries oldest first with a
/// running balance, current position, and every purchase with open payable.
pub async fn account_statement(
    pool: &PgPool,
    supplier_id: Uuid,
) -> Result<SupplierAccountStatement, SupplierAccountError> {
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::supplier_exists_in(&mut tx, supplier_id).await? {
        return Err(SupplierAccountError::SupplierNotFound);
    }

    let position = repository::account_position_in(&mut tx, supplier_id).await?;
    let raw_entries = repository::list_entries_in(&mut tx, supplier_id).await?;
    let outstanding_purchases =
        repository::outstanding_by_purchase_in(&mut tx, supplier_id).await?;
    tx.commit().await?;

    let mut running = Decimal::ZERO;
    let entries = raw_entries
        .into_iter()
        .map(|entry| {
            running += entry.amount;
            SupplierAccountStatementEntry {
                entry,
                running_balance: running,
            }
        })
        .collect();

    Ok(SupplierAccountStatement {
        supplier_id,
        position,
        entries,
        outstanding_purchases,
    })
}

/// Payment write log (`POST /suppliers/{id}/payments`), newest first.
pub async fn list_payments(
    pool: &PgPool,
    supplier_id: Uuid,
    limit: i64,
) -> Result<Vec<SupplierPaymentLogItem>, SupplierAccountError> {
    let limit = limit.clamp(1, 200);
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::supplier_exists_in(&mut tx, supplier_id).await? {
        return Err(SupplierAccountError::SupplierNotFound);
    }

    let payments = repository::list_payments_in(&mut tx, supplier_id, limit).await?;
    tx.commit().await?;
    Ok(payments)
}

/// Manual adjustment write log (`POST /suppliers/{id}/adjustments`), newest
/// first.
pub async fn list_adjustments(
    pool: &PgPool,
    supplier_id: Uuid,
    limit: i64,
) -> Result<Vec<SupplierAdjustmentLogItem>, SupplierAccountError> {
    let limit = limit.clamp(1, 200);
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::supplier_exists_in(&mut tx, supplier_id).await? {
        return Err(SupplierAccountError::SupplierNotFound);
    }

    let adjustments = repository::list_adjustments_in(&mut tx, supplier_id, limit).await?;
    tx.commit().await?;
    Ok(adjustments)
}

/// Portfolio payables report: suppliers with `balance != 0` or
/// `unapplied_credit != 0`, aging buckets from open purchases, and totals
/// summed across all listed suppliers in one consistent read-only snapshot.
pub async fn payables(pool: &PgPool) -> Result<PayablesSummary, SupplierAccountError> {
    let mut tx = begin_read_snapshot(pool).await?;
    let items = repository::list_payables_in(&mut tx).await?;
    tx.commit().await?;

    let mut total_balance = Decimal::ZERO;
    let mut total_unapplied_credit = Decimal::ZERO;
    let mut total_outstanding = Decimal::ZERO;
    let mut total_adjustments = Decimal::ZERO;
    let mut aging = AgingBuckets::default();

    for item in &items {
        total_balance += item.balance;
        total_unapplied_credit += item.unapplied_credit;
        total_outstanding += item.outstanding_total;
        total_adjustments += item.adjustments;
        aging.add(&item.aging);
    }

    Ok(PayablesSummary {
        total_balance,
        total_unapplied_credit,
        total_outstanding,
        total_adjustments,
        aging,
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn validate_amount_enforces_sign_precision_and_magnitude() {
        assert!(validate_amount(dec!(0.01)).is_ok());
        assert!(validate_amount(max_amount()).is_ok());
        assert!(matches!(
            validate_amount(dec!(0.00)),
            Err(SupplierAccountError::Validation(
                RepoError::NonPositiveAmount
            ))
        ));
        assert!(matches!(
            validate_amount(dec!(-5.00)),
            Err(SupplierAccountError::Validation(
                RepoError::NonPositiveAmount
            ))
        ));
        assert!(matches!(
            validate_amount(dec!(1.234)),
            Err(SupplierAccountError::Validation(
                RepoError::TooManyDecimals { .. }
            ))
        ));
        assert!(matches!(
            validate_amount(max_amount() + dec!(0.01)),
            Err(SupplierAccountError::AmountTooLarge { .. })
        ));
    }
}
