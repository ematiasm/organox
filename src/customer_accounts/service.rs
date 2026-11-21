//! Customer account use cases: collections, credit refunds and manual
//! adjustments.
//!
//! One transaction, one order: the customer lock first (S5), the replay
//! lookup under it and before anything is written, then the method, the
//! wallet movement and the ledger entry. A rejection anywhere rolls the whole
//! transaction back — the wallet never moves money the ledger refused to book,
//! and the ledger never books money no wallet saw.

use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::customer_accounts::model::{
    AccountStatement, AccountStatementEntry, AdjustmentLogItem, AgingBuckets,
    CustomerAccountEntryKind, ReceiptLogItem, ReceivablesSummary,
};
use crate::customer_accounts::repository::{self, RepoError};
use crate::payments::model::WalletMovementKind;
use crate::payments::repository as wallets;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptsError {
    #[error("the payment method is unknown or inactive")]
    MethodUnavailable,

    #[error("customer not found")]
    CustomerNotFound,

    #[error("sale not found")]
    SaleNotFound,

    #[error("the customer account is busy, retry")]
    CustomerBusy,

    #[error("that document is already recorded in the ledger")]
    AlreadyRecorded,

    #[error("the amount must be at most {max}")]
    AmountTooLarge { max: Decimal },

    #[error("a credit refund needs a reason")]
    RefundNoteRequired,

    /// The ledger's own typed rejections, each with a sentence the operator
    /// can act on: a bad amount, a sale that is not payable, allocations
    /// beyond what a sale owes or the receipt collected, a refund beyond
    /// usable credit, or an adjustment that crosses zero.
    #[error("{0}")]
    Validation(RepoError),

    #[error("internal: {0}")]
    Internal(RepoError),
}

impl From<RepoError> for ReceiptsError {
    /// Domain-relevant ledger failures keep their identity so the handler can
    /// answer 404, 409 or 422 instead of a blanket 500; everything else is the
    /// server's problem, not the client's.
    fn from(err: RepoError) -> Self {
        match err {
            RepoError::CustomerNotFound => ReceiptsError::CustomerNotFound,
            RepoError::SaleNotFound => ReceiptsError::SaleNotFound,
            RepoError::CustomerBusy => ReceiptsError::CustomerBusy,
            RepoError::AlreadyRecorded => ReceiptsError::AlreadyRecorded,
            RepoError::NonPositiveAmount
            | RepoError::TooManyDecimals { .. }
            | RepoError::SaleOfAnotherCustomer
            | RepoError::SaleNotChargeable
            | RepoError::OutstandingExceeded { .. }
            | RepoError::ReceiptOverAllocated { .. }
            | RepoError::DuplicateAllocation { .. }
            | RepoError::MovementOfAnotherCustomer
            | RepoError::CreditRefundExceedsCredit { .. }
            | RepoError::AdjustmentCrossesZero { .. }
            | RepoError::AdjustmentNoteRequired
            | RepoError::NotAnAdjustment => ReceiptsError::Validation(err),
            other => ReceiptsError::Internal(other),
        }
    }
}

impl From<sqlx::Error> for ReceiptsError {
    /// A transaction that fails to open or commit is infrastructure, never a
    /// rule: the client did nothing wrong and cannot be told what to fix.
    fn from(err: sqlx::Error) -> Self {
        ReceiptsError::Internal(RepoError::Database(err))
    }
}

impl From<wallets::RepoError> for ReceiptsError {
    /// The payments repository's failures are the server's problem here: the
    /// method's existence is decided by the `Option` it returns, not by an
    /// error, and a movement insert that fails is a database problem — the
    /// endpoint's own rules have already passed by then.
    fn from(err: wallets::RepoError) -> Self {
        match err {
            wallets::RepoError::Database(err) => ReceiptsError::Internal(RepoError::Database(err)),
            other => ReceiptsError::Internal(RepoError::Database(sqlx::Error::Protocol(format!(
                "payments repository: {other}"
            )))),
        }
    }
}

/// `numeric(14, 2)` cannot hold more than this; validating here turns a
/// database overflow (a 500) into a 422 that names the problem. The same
/// lesson the credit limit learned in review: the boundary owns the column's
/// magnitude.
fn max_amount() -> Decimal {
    // 12 integer digits and 2 decimals, the exact capacity of numeric(14, 2).
    Decimal::from_i128_with_scale(99_999_999_999_999, 2)
}

/// Shared money validation at the boundary, in published precedence order:
/// sign, then precision, then column magnitude.
fn validate_amount(amount: Decimal) -> Result<(), ReceiptsError> {
    if amount <= Decimal::ZERO {
        return Err(ReceiptsError::Validation(RepoError::NonPositiveAmount));
    }
    if amount.round_dp(2) != amount {
        return Err(ReceiptsError::Validation(RepoError::TooManyDecimals {
            amount,
        }));
    }
    if amount > max_amount() {
        return Err(ReceiptsError::AmountTooLarge { max: max_amount() });
    }
    Ok(())
}

/// One collection as the operator decided it. The allocations are theirs, not
/// something the server invents; the remainder of `amount` carries no row
/// anywhere and that is exactly what an available credit is.
pub struct CollectInput<'a> {
    pub payment_method_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub actor: Uuid,
}

/// The receipt as recorded (or as it already was), plus what the account says
/// right after: the balance, the usable credit, and what each allocated sale
/// still owes.
pub struct CollectedReceipt {
    pub entry_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
    /// Each allocated sale and its outstanding amount after the receipt.
    pub allocations: Vec<(Uuid, Decimal)>,
    pub replayed: bool,
}

/// The balance, the usable credit and what each allocated sale still owes,
/// read inside the transaction that answered the request, so the response
/// reports one consistent snapshot rather than two reads that could straddle
/// a commit.
async fn position_with_outstanding(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_ids: impl Iterator<Item = Uuid>,
) -> Result<(Decimal, Decimal, Vec<(Uuid, Decimal)>), RepoError> {
    let position = repository::account_position_in(conn, customer_id).await?;
    let mut outstanding = Vec::new();
    for sale_id in sale_ids {
        let owed = repository::outstanding_for_sale_in(conn, sale_id).await?;
        outstanding.push((sale_id, owed));
    }
    Ok((position.balance, position.unapplied_credit, outstanding))
}

/// Collect money against a customer's account.
///
/// The replay is answered before anything is written, under the lock: the
/// first receipt already consumed the money a retry would claim, so only the
/// recorded entry — not the request that retried it — can be the answer. The
/// lookup happening under the same lock as the write is what makes it
/// race-free: two copies of one request serialize, and the second sees the
/// first.
///
/// `record_receipt` re-locks and re-validates inside the same transaction:
/// harmless (an xact-scoped advisory lock is free the second time) and
/// deliberate — the ledger owns its own rules, and this endpoint never
/// trusts its own read of them.
pub async fn collect(
    pool: &PgPool,
    customer_id: Uuid,
    input: CollectInput<'_>,
) -> Result<CollectedReceipt, ReceiptsError> {
    // The ledger rejects these too, but only after the movement exists: a
    // boundary that publishes a 422 owns the sentence, and the transaction is
    // better off never opening.
    validate_amount(input.amount)?;

    let mut tx = pool.begin().await?;

    // One lock, taken first: every balance-changing writer starts here, so a
    // collection cannot race a completion or another collection. The same
    // read answers the 404: an unknown customer is refused before any method
    // or money is even looked at.
    repository::lock_customer(&mut tx, customer_id).await?;

    // A replay is answered before anything is written, and the lookup runs
    // under the lock taken above: two copies of one request serialize here,
    // and the second one sees the first one's receipt instead of racing it.
    if let Some(key) = input.idempotency_key
        && let Some(existing) = repository::receipt_by_key_in(&mut tx, customer_id, key).await?
    {
        // The sales the first receipt paid are read back from its own
        // allocations, not re-derived from the request: a replay answers with
        // what happened, not with what was asked the second time. The balance
        // and the credit are the account as it stands when the retry arrives —
        // another writer may have moved it since — which is what the response
        // documents.
        let (balance, unapplied_credit, allocations) =
            position_with_outstanding(&mut tx, customer_id, existing.sale_ids.iter().copied())
                .await?;

        // A commit, not a rollback: nothing was written either way, and the
        // connection simply goes back to the pool.
        tx.commit().await?;

        return Ok(CollectedReceipt {
            entry_id: existing.entry_id,
            wallet_movement_id: existing.wallet_movement_id,
            balance,
            unapplied_credit,
            allocations,
            replayed: true,
        });
    }

    // The method and its wallet are resolved under the lock, in the same
    // transaction that writes the movement: a method switched off between
    // the read and the write cannot collect money it may no longer take.
    let method = wallets::find_method(&mut *tx, input.payment_method_id)
        .await?
        .ok_or(ReceiptsError::MethodUnavailable)?;
    if !method.is_active || !method.wallet_is_active {
        return Err(ReceiptsError::MethodUnavailable);
    }

    // Exactly one movement for the money as collected. No sale, no credit
    // note: the allocations of the receipt say which sales it paid, and a sale
    // link here would double-count money the till already recorded.
    let movement_id = wallets::insert_movement(
        &mut *tx,
        wallets::NewWalletMovement {
            wallet_id: method.wallet_id,
            kind: WalletMovementKind::AccountPayment,
            amount: input.amount,
            sale_id: None,
            payment_method_id: Some(method.id),
            credit_note_id: None,
            customer_id: Some(customer_id),
            note: input.note,
            created_by: Some(input.actor),
        },
    )
    .await?;

    let recorded = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id,
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
        customer_id,
        input.allocations.iter().map(|(sale_id, _)| *sale_id),
    )
    .await?;

    tx.commit().await?;

    Ok(CollectedReceipt {
        entry_id: recorded.entry_id,
        wallet_movement_id: movement_id,
        balance,
        unapplied_credit,
        allocations,
        replayed: recorded.replayed,
    })
}

// --- Credit refunds (S10) -----------------------------------------------

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

/// Hand collected credit back to a customer through an `account_payment_refund`
/// wallet movement and a `credit_refund` ledger entry, in one transaction.
///
/// The replay lookup runs under the customer lock before anything is written,
/// mirroring [`collect`]: two copies of the same retry serialize, and the
/// second returns the first refund with `replayed = true`.
pub async fn refund_credit(
    pool: &PgPool,
    customer_id: Uuid,
    input: RefundCreditInput<'_>,
) -> Result<RefundedCredit, ReceiptsError> {
    validate_amount(input.amount)?;
    let note = input.note.trim();
    if note.is_empty() {
        return Err(ReceiptsError::RefundNoteRequired);
    }

    let mut tx = pool.begin().await?;
    repository::lock_customer(&mut tx, customer_id).await?;

    if let Some(key) = input.idempotency_key
        && let Some(existing) =
            repository::credit_refund_by_key_in(&mut tx, customer_id, key).await?
    {
        let position = repository::account_position_in(&mut tx, customer_id).await?;
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
        .ok_or(ReceiptsError::MethodUnavailable)?;
    if !method.is_active || !method.wallet_is_active {
        return Err(ReceiptsError::MethodUnavailable);
    }

    let movement_id = wallets::insert_movement(
        &mut *tx,
        wallets::NewWalletMovement {
            wallet_id: method.wallet_id,
            kind: WalletMovementKind::AccountPaymentRefund,
            amount: input.amount,
            sale_id: None,
            payment_method_id: Some(method.id),
            credit_note_id: None,
            customer_id: Some(customer_id),
            note: Some(note),
            created_by: Some(input.actor),
        },
    )
    .await?;

    let entry_id = repository::record_credit_refund_with_key(
        &mut tx,
        repository::NewCreditRefund {
            customer_id,
            wallet_movement_id: movement_id,
            amount: input.amount,
            idempotency_key: input.idempotency_key,
            note: Some(note),
            created_by: input.actor,
        },
    )
    .await?;

    let position = repository::account_position_in(&mut tx, customer_id).await?;
    tx.commit().await?;

    Ok(RefundedCredit {
        entry_id,
        wallet_movement_id: movement_id,
        balance: position.balance,
        unapplied_credit: position.unapplied_credit,
        replayed: false,
    })
}

// --- Manual adjustments (S11) -------------------------------------------

pub struct AdjustInput<'a> {
    pub kind: CustomerAccountEntryKind,
    pub amount: Decimal,
    pub note: &'a str,
    pub actor: Uuid,
}

pub struct RecordedAdjustment {
    pub entry_id: Uuid,
    pub kind: CustomerAccountEntryKind,
    pub balance: Decimal,
    pub unapplied_credit: Decimal,
}

/// Record a manual `adjustment_in` or `adjustment_out` without touching any
/// wallet. The ledger enforces that the balance stays non-negative.
pub async fn adjust(
    pool: &PgPool,
    customer_id: Uuid,
    input: AdjustInput<'_>,
) -> Result<RecordedAdjustment, ReceiptsError> {
    let note = input.note.trim();
    if note.is_empty() {
        return Err(ReceiptsError::Validation(RepoError::AdjustmentNoteRequired));
    }
    validate_amount(input.amount)?;

    let mut tx = pool.begin().await?;
    let entry_id = repository::record_adjustment(
        &mut tx,
        customer_id,
        input.kind,
        input.amount,
        note,
        input.actor,
    )
    .await?;
    let position = repository::account_position_in(&mut tx, customer_id).await?;
    tx.commit().await?;

    Ok(RecordedAdjustment {
        entry_id,
        kind: input.kind,
        balance: position.balance,
        unapplied_credit: position.unapplied_credit,
    })
}

// --- Read use cases (S13) -----------------------------------------------

/// Open a read-only repeatable-read transaction so multi-query read surfaces
/// observe one consistent ledger snapshot without blocking concurrent writers.
async fn begin_read_snapshot(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, ReceiptsError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Complete statement for one customer: ledger entries oldest first with a
/// running balance, current position, and every sale with open debt.
pub async fn account_statement(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<AccountStatement, ReceiptsError> {
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::customer_exists_in(&mut tx, customer_id).await? {
        return Err(ReceiptsError::CustomerNotFound);
    }

    let position = repository::account_position_in(&mut tx, customer_id).await?;
    let raw_entries = repository::list_entries_in(&mut tx, customer_id).await?;
    let outstanding_sales = repository::outstanding_by_sale_in(&mut tx, customer_id).await?;
    tx.commit().await?;

    let mut running = Decimal::ZERO;
    let entries = raw_entries
        .into_iter()
        .map(|entry| {
            running += entry.amount;
            AccountStatementEntry {
                entry,
                running_balance: running,
            }
        })
        .collect();

    Ok(AccountStatement {
        customer_id,
        position,
        entries,
        outstanding_sales,
    })
}

/// Collection write log (`POST /customers/{id}/receipts`), newest first.
pub async fn list_receipts(
    pool: &PgPool,
    customer_id: Uuid,
    limit: i64,
) -> Result<Vec<ReceiptLogItem>, ReceiptsError> {
    let limit = limit.clamp(1, 200);
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::customer_exists_in(&mut tx, customer_id).await? {
        return Err(ReceiptsError::CustomerNotFound);
    }

    let receipts = repository::list_receipts_in(&mut tx, customer_id, limit).await?;
    tx.commit().await?;
    Ok(receipts)
}

/// Manual adjustment write log (`POST /customers/{id}/adjustments`), newest
/// first.
pub async fn list_adjustments(
    pool: &PgPool,
    customer_id: Uuid,
    limit: i64,
) -> Result<Vec<AdjustmentLogItem>, ReceiptsError> {
    let limit = limit.clamp(1, 200);
    let mut tx = begin_read_snapshot(pool).await?;

    if !repository::customer_exists_in(&mut tx, customer_id).await? {
        return Err(ReceiptsError::CustomerNotFound);
    }

    let adjustments = repository::list_adjustments_in(&mut tx, customer_id, limit).await?;
    tx.commit().await?;
    Ok(adjustments)
}

/// Portfolio receivables report: customers with `balance != 0` or
/// `unapplied_credit != 0`, aging buckets from open sales, and totals summed
/// across all listed customers.
pub async fn receivables(pool: &PgPool) -> Result<ReceivablesSummary, ReceiptsError> {
    let items = repository::list_receivables(pool).await?;

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

    Ok(ReceivablesSummary {
        total_balance,
        total_unapplied_credit,
        total_outstanding,
        total_adjustments,
        aging,
        items,
    })
}
