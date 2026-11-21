//! Customer accounts data access.
//!
//! Two sums carry this module: the balance is `SUM(amount)` over the entries,
//! and a sale's outstanding amount is its charge minus the notes that cancelled
//! part of it minus the receipts allocated to it. Neither is stored, so neither
//! can drift.
//!
//! `unapplied_credit` is the third: money collected, not allocated to a sale and
//! not handed back. It is what an operator can still apply, and the reason a
//! customer can owe on one sale while holding money for the next.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
use std::collections::HashSet;
use uuid::Uuid;

use super::model::{AccountPosition, CustomerAccountEntryKind, SaleOutstanding};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error("customer not found")]
    CustomerNotFound,

    #[error("sale not found")]
    SaleNotFound,

    #[error("account entry not found")]
    EntryNotFound,

    #[error("that sale belongs to another customer")]
    SaleOfAnotherCustomer,

    #[error("that sale is not completed")]
    SaleNotChargeable,

    #[error("this sale is already charged to the account")]
    AlreadyCharged,

    #[error("the amount must be positive")]
    NonPositiveAmount,

    #[error("the amount {amount} carries more than two decimals")]
    TooManyDecimals { amount: Decimal },

    #[error("the allocation of {requested} exceeds the {outstanding} this sale still owes")]
    OutstandingExceeded {
        requested: Decimal,
        outstanding: Decimal,
    },

    #[error("the allocations ({allocated}) exceed the money the receipt collected ({collected})")]
    ReceiptOverAllocated {
        allocated: Decimal,
        collected: Decimal,
    },

    #[error("this receipt collected {collected} and {allocated} is already allocated")]
    AllocationExceedsReceipt {
        collected: Decimal,
        allocated: Decimal,
    },

    #[error("the allocation of {requested} exceeds the {available} the customer holds in credit")]
    AllocationExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("only a receipt can be allocated to a sale")]
    NotAReceipt,

    #[error("wallet movement not found")]
    MovementNotFound,

    #[error("the receipt records {receipt} but the wallet movement carries {movement}")]
    ReceiptDoesNotMatchMovement { movement: Decimal, receipt: Decimal },

    #[error("the refund of {requested} exceeds the {available} the customer holds in credit")]
    CreditRefundExceedsCredit {
        requested: Decimal,
        available: Decimal,
    },

    #[error("an adjustment cannot cross zero: the balance is {balance}")]
    AdjustmentCrossesZero { balance: Decimal },

    #[error("that kind is not an adjustment")]
    NotAnAdjustment,

    #[error("an adjustment needs a reason")]
    AdjustmentNoteRequired,

    #[error("that document is already recorded in the ledger")]
    AlreadyRecorded,

    #[error("the charge of {requested} exceeds the {available} the sale leaves unpaid")]
    ChargeExceedsSaleTotal {
        requested: Decimal,
        available: Decimal,
    },

    #[error("credit note not found")]
    CreditNoteNotFound,

    #[error("that credit note belongs to another sale")]
    CreditNoteMismatch,

    #[error("the note cancels {requested} but the note is only {available}")]
    CreditNoteExceeded {
        requested: Decimal,
        available: Decimal,
    },

    #[error("the same sale appears twice in one receipt")]
    DuplicateAllocation { sale_id: Uuid },

    #[error("a receipt cannot be backed by a {kind} movement")]
    MovementNotFromAnAccount { kind: String },

    #[error("the customer account is busy, retry")]
    CustomerBusy,

    #[error("the customer is deactivated")]
    CustomerInactive,

    #[error("the charge of {requested} exceeds the {available} the customer still has available")]
    CreditLimitExceeded {
        requested: Decimal,
        available: Decimal,
    },
}

/// The customer's account as of now: balance, usable credit, what is owed.
pub async fn account_position(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<AccountPosition, RepoError> {
    let mut conn = pool.acquire().await?;
    account_position_in(&mut conn, customer_id).await
}

/// [`account_position`] inside an open transaction.
///
/// One statement, one snapshot: the aggregates and `outstanding_total` are
/// read at the same moment of the ledger, so a position read outside the
/// customer lock can no longer show a balance from one instant and a debt
/// total from the next. The scalar subquery repeats the definition
/// [`outstanding_by_sale_in`] lists — per sale, charge minus cancelled minus
/// allocated, positive rows only — so the two surfaces cannot disagree.
pub async fn account_position_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<AccountPosition, RepoError> {
    let row = sqlx::query!(
        r#"
        WITH entries AS (
            SELECT COALESCE(SUM(amount), 0) AS balance,
                   COALESCE(SUM(CASE WHEN kind = 'receipt' THEN -amount ELSE 0 END), 0)
                       AS received,
                   COALESCE(SUM(CASE WHEN kind = 'credit_refund' THEN amount ELSE 0 END), 0)
                       AS refunded,
                   COALESCE(SUM(CASE WHEN kind IN ('adjustment_in', 'adjustment_out')
                                     THEN amount ELSE 0 END), 0)
                       AS adjustments
            FROM customer_account_entries
            WHERE customer_id = $1
        ),
        allocated AS (
            SELECT COALESCE(SUM(a.amount), 0) AS total
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
        ),
        charges AS (
            SELECT sale_id, SUM(amount) AS charged
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'sale_charge'
            GROUP BY sale_id
        ),
        cancelled AS (
            SELECT sale_id, -SUM(amount) AS cancelled
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'credit_note'
            GROUP BY sale_id
        ),
        sale_allocated AS (
            SELECT a.sale_id, SUM(a.amount) AS allocated
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
            GROUP BY a.sale_id
        ),
        debts AS (
            SELECT c.charged - COALESCE(x.cancelled, 0) - COALESCE(l.allocated, 0)
                AS outstanding
            FROM charges c
            LEFT JOIN cancelled x ON x.sale_id = c.sale_id
            LEFT JOIN sale_allocated l ON l.sale_id = c.sale_id
        )
        SELECT balance::numeric(14, 2)                       AS "balance!: Decimal",
               (received - refunded - allocated.total)::numeric(14, 2)
                                                             AS "unapplied_credit!: Decimal",
               adjustments::numeric(14, 2)                   AS "adjustments!: Decimal",
               COALESCE((SELECT SUM(outstanding) FROM debts WHERE outstanding > 0), 0)
                   ::numeric(14, 2)                          AS "outstanding_total!: Decimal"
        FROM entries, allocated
        "#,
        customer_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(AccountPosition {
        balance: row.balance,
        unapplied_credit: row.unapplied_credit,
        outstanding_total: row.outstanding_total,
        adjustments: row.adjustments,
    })
}

/// Every sale of this customer that still owes money, oldest first.
pub async fn outstanding_by_sale(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<Vec<SaleOutstanding>, RepoError> {
    let mut conn = pool.acquire().await?;
    outstanding_by_sale_in(&mut conn, customer_id).await
}

/// [`outstanding_by_sale`] inside an open transaction.
pub async fn outstanding_by_sale_in(
    conn: &mut PgConnection,
    customer_id: Uuid,
) -> Result<Vec<SaleOutstanding>, RepoError> {
    let rows = sqlx::query!(
        r#"
        WITH charges AS (
            SELECT sale_id, SUM(amount) AS charged
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'sale_charge'
            GROUP BY sale_id
        ),
        cancelled AS (
            SELECT sale_id, -SUM(amount) AS cancelled
            FROM customer_account_entries
            WHERE customer_id = $1 AND kind = 'credit_note'
            GROUP BY sale_id
        ),
        allocated AS (
            SELECT a.sale_id, SUM(a.amount) AS allocated
            FROM customer_account_allocations a
            JOIN customer_account_entries e ON e.id = a.entry_id
            WHERE e.customer_id = $1
            GROUP BY a.sale_id
        )
        SELECT c.sale_id                             AS "sale_id!: Uuid",
               s.completed_at                        AS "completed_at!: DateTime<Utc>",
               c.charged::numeric(14, 2)             AS "charged!: Decimal",
               COALESCE(x.cancelled, 0)::numeric(14, 2) AS "cancelled!: Decimal",
               COALESCE(l.allocated, 0)::numeric(14, 2) AS "allocated!: Decimal",
               (c.charged
                    - COALESCE(x.cancelled, 0)
                    - COALESCE(l.allocated, 0))::numeric(14, 2) AS "outstanding!: Decimal"
        FROM charges c
        JOIN sales s ON s.id = c.sale_id
        LEFT JOIN cancelled x ON x.sale_id = c.sale_id
        LEFT JOIN allocated l ON l.sale_id = c.sale_id
        WHERE c.charged - COALESCE(x.cancelled, 0) - COALESCE(l.allocated, 0) > 0
        ORDER BY s.completed_at, c.sale_id
        "#,
        customer_id
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SaleOutstanding {
            sale_id: row.sale_id,
            completed_at: row.completed_at,
            charged: row.charged,
            cancelled: row.cancelled,
            allocated: row.allocated,
            outstanding: row.outstanding,
        })
        .collect())
}

// --- Writes ------------------------------------------------------------

/// The constraint that rejected the row, when the error came from the database.
fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db_err) => db_err.constraint(),
        _ => None,
    }
}

/// An amount a money writer accepts: positive, and no finer than cents. The
/// column is `numeric(14, 2)`, so Postgres would silently round a third
/// decimal instead of rejecting it — and a ledger that rounds what the
/// operator typed is a ledger nobody can audit.
fn ensure_cents(amount: Decimal) -> Result<(), RepoError> {
    if amount <= Decimal::ZERO {
        return Err(RepoError::NonPositiveAmount);
    }
    if amount.round_dp(2) != amount {
        return Err(RepoError::TooManyDecimals { amount });
    }
    Ok(())
}

/// What one sale still owes, on the same definition [`outstanding_by_sale_in`]
/// lists: its charge, minus the notes that cancelled part of it, minus the
/// receipts allocated to it. Derived per sale, because a writer has to check
/// one allocation without reading the customer's whole debt list.
pub async fn outstanding_for_sale_in(
    conn: &mut PgConnection,
    sale_id: Uuid,
) -> Result<Decimal, RepoError> {
    let outstanding = sqlx::query_scalar!(
        r#"
        WITH charge AS (
            SELECT COALESCE(SUM(amount), 0) AS charged
            FROM customer_account_entries
            WHERE sale_id = $1 AND kind = 'sale_charge'
        ),
        cancelled AS (
            SELECT COALESCE(-SUM(amount), 0) AS cancelled
            FROM customer_account_entries
            WHERE sale_id = $1 AND kind = 'credit_note'
        ),
        allocated AS (
            SELECT COALESCE(SUM(amount), 0) AS allocated
            FROM customer_account_allocations
            WHERE sale_id = $1
        )
        SELECT (charge.charged - cancelled.cancelled - allocated.allocated)::numeric(14, 2)
               AS "outstanding!: Decimal"
        FROM charge, cancelled, allocated
        "#,
        sale_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(outstanding)
}

/// One writer, one lock, taken first. The lock is deliberately not the
/// customer's row: the sale-completion transaction already locks the sale and
/// its products, so a collection that locked the customer row first could
/// deadlock against it. An xact-scoped advisory lock gives every
/// balance-changing writer the same first step, and concurrent collections
/// serialize here instead of racing on the sums.
/// One writer, one lock, taken first. The lock is deliberately not the
/// customer's row: the sale-completion transaction already locks the sale and
/// its products, so a collection that locked the customer row first could
/// deadlock against it. An xact-scoped advisory lock gives every
/// balance-changing writer the same first step, and concurrent collections
/// serialize here instead of racing on the sums.
///
/// The wait is bounded: a writer that never finished would otherwise park
/// every later collection behind it forever, so the lock times out and the
/// caller answers "busy, retry". `SET LOCAL` keeps the timeout inside this
/// transaction, and the session default is restored right after — the
/// connection goes back to the pool and must not carry a timeout the pool
/// never set.
pub async fn lock_customer(conn: &mut PgConnection, customer_id: Uuid) -> Result<(), RepoError> {
    let found = sqlx::query!(
        r#"SELECT 1 AS found FROM customers WHERE id = $1"#,
        customer_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    if found.is_none() {
        return Err(RepoError::CustomerNotFound);
    }

    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *conn)
        .await?;

    let locked = sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        customer_id.to_string()
    )
    .execute(&mut *conn)
    .await;

    if let Err(err) = locked {
        // 55P03 is `lock_not_available`: the timeout decided, not a failure.
        if matches!(&err, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03")) {
            return Err(RepoError::CustomerBusy);
        }
        return Err(err.into());
    }

    sqlx::query!("SET LOCAL lock_timeout = 0")
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Authorize one charge before it is written: the gate a completion passes
/// through, and the only place the registry's decisions about credit are
/// enforced.
///
/// The order of the two locks is the whole point. The advisory lock is taken
/// first, so this gate serializes with every other balance-changing writer on
/// the account; then the customer's row is read `FOR UPDATE`, so a
/// deactivation racing the checkout cannot slip between the gate and the
/// charge — it waits for the row lock, and when it finally runs the next
/// completion sees `is_active = false`. The two orders cannot invert, because
/// a deactivation takes only the row lock and never the advisory one.
///
/// The rejection is on exposure, not on the single sale: `balance + amount`
/// against `credit_limit`, where `NULL` means no limit at all and `0` means
/// cash only. A customer holding credit (a negative balance) absorbs the new
/// charge before the limit applies, with no special case: adding a negative
/// balance to the amount is exactly "spending money already in their favour",
/// and using that money is not credit. The message names what is left,
/// floored at zero because a customer whose exposure already exceeds the
/// limit has nothing available, not something negative.
///
/// The position is only read when a limit exists: with no limit the activity
/// gate above is the whole rule, and reading sums nobody will compare is
/// cost without a decision.
pub async fn authorize_credit(
    conn: &mut PgConnection,
    customer_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    lock_customer(conn, customer_id).await?;

    let customer = sqlx::query!(
        r#"
        SELECT is_active,
               credit_limit::numeric(14, 2) AS "credit_limit?: Decimal"
        FROM customers
        WHERE id = $1
        FOR UPDATE
        "#,
        customer_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::CustomerNotFound)?;

    if !customer.is_active {
        return Err(RepoError::CustomerInactive);
    }

    let Some(limit) = customer.credit_limit else {
        // No limit: the registry imposed no ceiling, only the activity rule.
        return Ok(());
    };

    let position = account_position_in(conn, customer_id).await?;

    let exposure = position.balance + amount;
    if exposure > limit {
        return Err(RepoError::CreditLimitExceeded {
            requested: exposure,
            available: (limit - position.balance).max(Decimal::ZERO),
        });
    }

    Ok(())
}

/// The sale an account operation may point at: it exists, it belongs to this
/// customer, and it is completed. One helper, three writers, so the three can
/// never disagree about what a payable sale is.
async fn ensure_chargeable_sale(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
) -> Result<(), RepoError> {
    let sale = sqlx::query!(
        r#"SELECT customer_id, status::text AS "status!" FROM sales WHERE id = $1"#,
        sale_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::SaleNotFound)?;

    if sale.customer_id != Some(customer_id) {
        return Err(RepoError::SaleOfAnotherCustomer);
    }
    if sale.status != "completed" {
        return Err(RepoError::SaleNotChargeable);
    }
    Ok(())
}

/// The movement a money-backed entry points at: it must exist, it must have
/// carried exactly the money the entry claims, and it must not be the till's
/// own accounting of a sale. The foreign key proves the movement exists; only
/// the comparison proves the two books agree on how much moved. And a
/// `sale_payment` or `sale_refund` movement collected for a sale at the till:
/// it is not money that came from the customer's account, so a receipt backed
/// by it would collect the same money twice. The kinds are compared by name
/// because the `account_payment*` kinds that will back these entries arrive
/// with the slices that write them and must pass here unchanged.
async fn ensure_movement_backing(
    conn: &mut PgConnection,
    wallet_movement_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    let movement = sqlx::query!(
        r#"SELECT amount, kind::text AS "kind!" FROM wallet_movements WHERE id = $1"#,
        wallet_movement_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::MovementNotFound)?;

    if movement.amount != amount {
        return Err(RepoError::ReceiptDoesNotMatchMovement {
            movement: movement.amount,
            receipt: amount,
        });
    }

    if movement.kind == "sale_payment" || movement.kind == "sale_refund" {
        return Err(RepoError::MovementNotFromAnAccount {
            kind: movement.kind,
        });
    }

    Ok(())
}

/// Charge one sale's unpaid part to the account. The unique partial index is
/// the real double-charge guard — the sale-completion path of a later slice
/// writes the same entry — so a retried completion cannot bill the same sale
/// twice even if it never goes through this function.
pub async fn record_charge(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
    amount: Decimal,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;
    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    // A charge may only carry what the sale still leaves unpaid: the wallet's
    // `sale_payment` movements already collected part of the total at the
    // till, and charging that part again would bill the customer for money
    // they already handed over. The completion slice will require exactly
    // this remainder as its `on_account` equality.
    let sale = sqlx::query!(
        r#"
        SELECT s.total::numeric(14, 2) AS "total!: Decimal",
               COALESCE(
                   (SELECT SUM(amount) FROM wallet_movements
                    WHERE sale_id = s.id AND kind = 'sale_payment'),
                   0
               )::numeric(14, 2) AS "collected!: Decimal"
        FROM sales s
        WHERE s.id = $1
        "#,
        sale_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let available = sale.total - sale.collected;
    if amount > available {
        return Err(RepoError::ChargeExceedsSaleTotal {
            requested: amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, created_by)
        VALUES ($1, $2, 'sale_charge', $3, $4, $5)
        "#,
        id,
        customer_id,
        amount,
        sale_id,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("customer_account_entries_one_charge_per_sale") {
            RepoError::AlreadyCharged
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// A collection: the movement that carried the money, and the sales it pays.
///
/// `allocations` is what the operator decided, never something the server
/// invents; the remainder of `amount` carries no row anywhere and that is
/// exactly what an available credit is.
pub struct NewReceipt<'a> {
    pub customer_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub amount: Decimal,
    pub allocations: &'a [(Uuid, Decimal)],
    pub idempotency_key: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Uuid,
}

/// The receipt as recorded, or as it already was: a replay answers the first
/// entry so a retried collection never collects twice.
pub struct RecordedReceipt {
    pub entry_id: Uuid,
    pub replayed: bool,
}

/// Record one collection and its allocations.
///
/// Everything is validated before the first insert, so a rejection leaves no
/// entry and no allocation behind — in an append-only ledger a rejected write
/// must not need a compensating entry. The advisory lock is also what makes
/// the replay lookup race-free: two concurrent copies of the same request
/// serialize, and the second one sees the first one's entry.
pub async fn record_receipt(
    conn: &mut PgConnection,
    receipt: NewReceipt<'_>,
) -> Result<RecordedReceipt, RepoError> {
    ensure_cents(receipt.amount)?;
    lock_customer(conn, receipt.customer_id).await?;

    ensure_movement_backing(conn, receipt.wallet_movement_id, receipt.amount).await?;

    // A repeated sale in one receipt is the same allocation twice. The
    // engine's `UNIQUE (entry_id, sale_id)` would refuse it, but as an
    // untyped database error and only after the entry had been written;
    // refusing it here keeps the rejection typed and the ledger untouched.
    let mut seen = HashSet::new();
    for (sale_id, _) in receipt.allocations {
        if !seen.insert(*sale_id) {
            return Err(RepoError::DuplicateAllocation { sale_id: *sale_id });
        }
    }

    if let Some(key) = receipt.idempotency_key {
        let first = sqlx::query_scalar!(
            r#"
            SELECT id FROM customer_account_entries
            WHERE customer_id = $1 AND idempotency_key = $2
            "#,
            receipt.customer_id,
            key,
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(entry_id) = first {
            return Ok(RecordedReceipt {
                entry_id,
                replayed: true,
            });
        }
    }

    // Each allocation is checked against what its sale still owes before
    // anything is written; the sum is checked last so a receipt that pays one
    // sale too much names the sale, not the sum.
    let mut allocated = Decimal::ZERO;
    for (sale_id, amount) in receipt.allocations {
        ensure_cents(*amount)?;
        ensure_chargeable_sale(conn, receipt.customer_id, *sale_id).await?;

        let outstanding = outstanding_for_sale_in(conn, *sale_id).await?;
        if *amount > outstanding {
            return Err(RepoError::OutstandingExceeded {
                requested: *amount,
                outstanding,
            });
        }
        allocated += *amount;
    }
    if allocated > receipt.amount {
        return Err(RepoError::ReceiptOverAllocated {
            allocated,
            collected: receipt.amount,
        });
    }

    let entry_id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, wallet_movement_id, idempotency_key, note, created_by)
        VALUES ($1, $2, 'receipt', $3, $4, $5, $6, $7)
        "#,
        entry_id,
        receipt.customer_id,
        -receipt.amount,
        receipt.wallet_movement_id,
        receipt.idempotency_key,
        receipt.note,
        receipt.created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| match violated_constraint(&err) {
        // Both mean this money is already in the ledger: the movement already
        // backs an entry, or this idempotency key already collected once.
        // The lock makes them rare; the typed mapping makes them readable.
        Some("customer_account_entries_one_entry_per_movement")
        | Some("customer_account_entries_receipt_idempotency") => RepoError::AlreadyRecorded,
        _ => RepoError::Database(err),
    })?;

    for (sale_id, amount) in receipt.allocations {
        sqlx::query!(
            r#"
            INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
            VALUES ($1, $2, $3, $4)
            "#,
            Uuid::now_v7(),
            entry_id,
            *sale_id,
            *amount,
        )
        .execute(&mut *conn)
        .await?;
    }

    Ok(RecordedReceipt {
        entry_id,
        replayed: false,
    })
}

/// Point already-collected money at one more sale. This is the primitive the
/// credit-application slice will call: it moves no balance, it only says which
/// sale the money pays.
pub async fn allocate(
    conn: &mut PgConnection,
    entry_id: Uuid,
    sale_id: Uuid,
    amount: Decimal,
) -> Result<(), RepoError> {
    ensure_cents(amount)?;

    // Read before locking: the entry is append-only, so this row cannot change
    // under us; only the allocation sum below needs the lock.
    let entry = sqlx::query!(
        r#"
        SELECT customer_id, kind AS "kind: CustomerAccountEntryKind", amount
        FROM customer_account_entries
        WHERE id = $1
        "#,
        entry_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::EntryNotFound)?;

    if entry.kind != CustomerAccountEntryKind::Receipt {
        return Err(RepoError::NotAReceipt);
    }

    lock_customer(conn, entry.customer_id).await?;

    let already = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0)::numeric(14, 2) AS "already!: Decimal"
        FROM customer_account_allocations
        WHERE entry_id = $1
        "#,
        entry_id
    )
    .fetch_one(&mut *conn)
    .await?;

    let collected = -entry.amount;
    if already + amount > collected {
        return Err(RepoError::AllocationExceedsReceipt {
            collected,
            allocated: already,
        });
    }

    ensure_chargeable_sale(conn, entry.customer_id, sale_id).await?;

    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

    // The receipt-level ceiling above is not enough on its own: money already
    // handed back as a credit refund still sits inside a receipt that its
    // per-receipt sum cannot see. Allocating it would dispose of the same money
    // twice — once as cash leaving a wallet, once as a settled sale — and the
    // balance identity would still hold, so only this customer-level ceiling
    // catches it. `unapplied_credit` is exactly `collected - allocated -
    // refunded`, which is what an allocation may still consume.
    let position = account_position_in(conn, entry.customer_id).await?;
    if amount > position.unapplied_credit {
        return Err(RepoError::AllocationExceedsCredit {
            requested: amount,
            available: position.unapplied_credit,
        });
    }

    sqlx::query!(
        r#"
        INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
        VALUES ($1, $2, $3, $4)
        "#,
        Uuid::now_v7(),
        entry_id,
        sale_id,
        amount,
    )
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Cancel part of a sale's debt with a credit note. The note is the document,
/// the entry is the ledger's copy of it: one note writes one entry, once, and
/// the unique index on `credit_note_id` is what keeps that true.
///
/// The note's own ceiling — that notes never exceed the sale's total, and how
/// the remainder is refunded in cash — belongs to the credit-note slice that
/// writes the note document. This writer refuses only what the ledger itself
/// forbids: cancelling debt that does not exist.
pub async fn record_credit_note(
    conn: &mut PgConnection,
    customer_id: Uuid,
    sale_id: Uuid,
    credit_note_id: Uuid,
    amount: Decimal,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;
    ensure_chargeable_sale(conn, customer_id, sale_id).await?;

    // The note is the document this entry copies, so the document decides:
    // it must be about this sale, and it cannot cancel more than it carries.
    // Checked before the ledger's own ceiling, because a note that never
    // belonged to the sale is a mismatch, not unaffordable debt.
    let note = sqlx::query!(
        r#"
        SELECT sale_id, total::numeric(14, 2) AS "total!: Decimal"
        FROM credit_notes
        WHERE id = $1
        "#,
        credit_note_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RepoError::CreditNoteNotFound)?;

    if note.sale_id != sale_id {
        return Err(RepoError::CreditNoteMismatch);
    }
    if amount > note.total {
        return Err(RepoError::CreditNoteExceeded {
            requested: amount,
            available: note.total,
        });
    }

    let outstanding = outstanding_for_sale_in(conn, sale_id).await?;
    if amount > outstanding {
        return Err(RepoError::OutstandingExceeded {
            requested: amount,
            outstanding,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
        VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)
        "#,
        id,
        customer_id,
        -amount,
        sale_id,
        credit_note_id,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("customer_account_entries_one_entry_per_note") {
            RepoError::AlreadyRecorded
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Hand collected money back to a customer who holds credit. The wallet
/// movement is the money actually leaving; the entry is the ledger's copy,
/// stored positive because a refund moves the balance toward zero.
///
/// Which wallet kind carried the money out is deliberately not this writer's
/// business: the movement's amount is the contract, and the kind that refunds
/// credit does not exist yet (it arrives with the slice that writes the
/// movement).
pub async fn record_credit_refund(
    conn: &mut PgConnection,
    customer_id: Uuid,
    wallet_movement_id: Uuid,
    amount: Decimal,
    note: Option<&str>,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;

    // The same checks the receipt makes, for the same reasons: the movement
    // carried what the entry claims, and it is not a sale's own till money.
    ensure_movement_backing(conn, wallet_movement_id, amount).await?;

    // A replay answers by identity, not by re-running the numbers: the first
    // refund already consumed the credit this one claims, so affordability
    // alone could not tell a replay from a fresh refund that happens to be
    // unaffordable. The lock makes the lookup race-free; the unique index
    // stays as the backstop.
    let already = sqlx::query_scalar!(
        r#"
        SELECT id FROM customer_account_entries
        WHERE wallet_movement_id = $1
        "#,
        wallet_movement_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if already.is_some() {
        return Err(RepoError::AlreadyRecorded);
    }

    let position = account_position_in(conn, customer_id).await?;

    // Money is only handed back against money the shop actually holds: the
    // unapplied credit says it was collected, the negative balance says it was
    // never spent on debt. Either alone could lie — unapplied credit next to
    // a debt, or a negative balance with the credit already refunded — so the
    // refund sees the smaller of the two, floored at zero because a customer
    // who owes has nothing to claim back at all.
    let available = position
        .unapplied_credit
        .min(-position.balance)
        .max(Decimal::ZERO);
    if amount > available {
        return Err(RepoError::CreditRefundExceedsCredit {
            requested: amount,
            available,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, wallet_movement_id, note, created_by)
        VALUES ($1, $2, 'credit_refund', $3, $4, $5, $6)
        "#,
        id,
        customer_id,
        amount,
        wallet_movement_id,
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some("customer_account_entries_one_entry_per_movement") {
            RepoError::AlreadyRecorded
        } else {
            RepoError::Database(err)
        }
    })?;

    Ok(id)
}

/// Record a manual adjustment: the only way to load an opening balance or
/// correct the books by hand. It moves the balance without touching any sale,
/// so it must never create or destroy an available credit — that credit is
/// backed by receipts, and only receipts and refunds may move it.
pub async fn record_adjustment(
    conn: &mut PgConnection,
    customer_id: Uuid,
    kind: CustomerAccountEntryKind,
    amount: Decimal,
    note: &str,
    created_by: Uuid,
) -> Result<Uuid, RepoError> {
    // The kind is checked before anything is read: an operator cannot smuggle
    // a charge or a receipt through the adjustment form.
    if !kind.is_manual() {
        return Err(RepoError::NotAnAdjustment);
    }
    if note.trim().is_empty() {
        return Err(RepoError::AdjustmentNoteRequired);
    }

    ensure_cents(amount)?;
    lock_customer(conn, customer_id).await?;

    let position = account_position_in(conn, customer_id).await?;

    // An adjustment operates on a debt, so it may only run while the balance is
    // a debt: entering more than is owed would mint credit out of nothing, and
    // adjusting while the customer holds credit would bury it under debt.
    if position.balance < Decimal::ZERO
        || (kind == CustomerAccountEntryKind::AdjustmentOut && amount > position.balance)
    {
        return Err(RepoError::AdjustmentCrossesZero {
            balance: position.balance,
        });
    }

    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO customer_account_entries (id, customer_id, kind, amount, note, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        id,
        customer_id,
        kind as CustomerAccountEntryKind,
        amount * Decimal::from(kind.sign()),
        note,
        created_by,
    )
    .execute(&mut *conn)
    .await?;

    Ok(id)
}
