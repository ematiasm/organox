//! The collection use case: money arriving into a wallet against a customer's
//! account, allocated to the sales it pays.
//!
//! One transaction, one order: the customer lock first (S5), the replay
//! lookup under it and before anything is written, then the method, the
//! `account_payment` movement and the ledger's receipt. A rejection anywhere
//! rolls the whole transaction back — the wallet never takes money the ledger
//! refused to book, and the ledger never books money no wallet saw.

use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

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

    /// The ledger's own typed rejections, each with a sentence the operator
    /// can act on: a bad amount, a sale that is not payable, or allocations
    /// beyond what a sale owes or the receipt collected.
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
            | RepoError::MovementOfAnotherCustomer => ReceiptsError::Validation(err),
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
    if input.amount <= Decimal::ZERO {
        return Err(ReceiptsError::Validation(RepoError::NonPositiveAmount));
    }
    if input.amount.round_dp(2) != input.amount {
        return Err(ReceiptsError::Validation(RepoError::TooManyDecimals {
            amount: input.amount,
        }));
    }
    if input.amount > max_amount() {
        return Err(ReceiptsError::AmountTooLarge { max: max_amount() });
    }

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
