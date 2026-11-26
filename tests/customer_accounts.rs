//! Customer account ledger tests.
//!
//! What these protect, in order of importance:
//!  1. Balance, usable credit and outstanding amounts always satisfy the
//!     invariant that ties the debt ledger to the sales it came from.
//!  2. The ledger is append-only, and no entry can be stored with the sign its
//!     kind forbids: a receipt stored positive would double a debt.
//!  3. A sale is charged once, a receipt cannot pay more than it collected, and
//!     it can never pay another customer's sale.
//!
//! These go through the SQL contract and the read model directly. The slice has
//! no HTTP surface yet, and a constraint test *has* to bypass any writer: a test
//! that inserts through the repository only proves the repository would not have
//! written the bad row, not that the database would have rejected it.
//!
//! Not covered here on purpose: the `credit_note` kind, which needs a real note
//! document and is pinned by the credit-note slice that writes it.

use std::time::Duration;

use organox::customer_accounts::model::CustomerAccountEntryKind;
use organox::customer_accounts::repository;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use uuid::Uuid;

// --- Fixtures -----------------------------------------------------------

async fn seed_customer(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO customers (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("customer");
    id
}

/// A completed sale with no items: enough to own a debt, nothing else.
async fn seed_completed_sale(pool: &PgPool, customer_id: Uuid, total: Decimal) -> Uuid {
    let id = Uuid::now_v7();
    let actor = Uuid::now_v7();
    let name: String = sqlx::query_scalar("SELECT name FROM customers WHERE id = $1")
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .expect("customer name");

    sqlx::query(
        r#"
        INSERT INTO sales (id, status, total, created_by, completed_by, completed_at,
                           customer_id, customer_name)
        VALUES ($1, 'completed', $2, $3, $3, now(), $4, $5)
        "#,
    )
    .bind(id)
    .bind(total)
    .bind(actor)
    .bind(customer_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("sale");

    id
}

async fn seed_draft_sale(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let actor = Uuid::now_v7();
    sqlx::query("INSERT INTO sales (id, created_by) VALUES ($1, $2)")
        .bind(id)
        .bind(actor)
        .execute(pool)
        .await
        .expect("draft sale");
    id
}

/// A wallet movement that belongs to a customer's account: raw SQL on purpose,
/// like every fixture here, so the ledger's own tests never depend on the
/// service layer above it.
async fn seed_customer_movement(
    pool: &PgPool,
    customer_id: Uuid,
    kind: &str,
    amount: Decimal,
) -> Uuid {
    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    let movement = Uuid::now_v7();
    let suffix = Uuid::new_v4();

    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
        .bind(wallet)
        .bind(format!("Wallet {suffix}"))
        .execute(pool)
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
        .bind(method)
        .bind(format!("Method {suffix}"))
        .bind(wallet)
        .execute(pool)
        .await
        .expect("method");
    sqlx::query(
        r#"
        INSERT INTO wallet_movements (id, wallet_id, kind, amount, customer_id, payment_method_id)
        VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)
        "#,
    )
    .bind(movement)
    .bind(wallet)
    .bind(kind)
    .bind(amount)
    .bind(customer_id)
    .bind(method)
    .execute(pool)
    .await
    .expect("customer account movement");

    movement
}

async fn seed_account_payment(pool: &PgPool, customer_id: Uuid, amount: Decimal) -> Uuid {
    seed_customer_movement(pool, customer_id, "account_payment", amount).await
}

async fn seed_account_payment_refund(pool: &PgPool, customer_id: Uuid, amount: Decimal) -> Uuid {
    seed_customer_movement(pool, customer_id, "account_payment_refund", amount).await
}

struct NewEntry<'a> {
    customer: Uuid,
    kind: &'a str,
    amount: Decimal,
    sale: Option<Uuid>,
    movement: Option<Uuid>,
    note: Option<&'a str>,
}

impl<'a> NewEntry<'a> {
    fn charge(customer: Uuid, sale: Uuid, amount: Decimal) -> Self {
        Self {
            customer,
            kind: "sale_charge",
            amount,
            sale: Some(sale),
            movement: None,
            note: None,
        }
    }

    fn receipt(customer: Uuid, movement: Uuid, amount: Decimal) -> Self {
        Self {
            customer,
            kind: "receipt",
            amount: -amount,
            sale: None,
            movement: Some(movement),
            note: None,
        }
    }

    fn adjustment(customer: Uuid, amount: Decimal, note: &'a str) -> Self {
        Self {
            customer,
            kind: "adjustment_in",
            amount,
            sale: None,
            movement: None,
            note: Some(note),
        }
    }
}

async fn insert_entry<'e, E>(executor: E, entry: NewEntry<'_>) -> Result<Uuid, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let id = Uuid::now_v7();
    let actor = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, sale_id, wallet_movement_id, note, created_by)
        VALUES ($1, $2, $3::customer_account_entry_kind, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(id)
    .bind(entry.customer)
    .bind(entry.kind)
    .bind(entry.amount)
    .bind(entry.sale)
    .bind(entry.movement)
    .bind(entry.note)
    .bind(actor)
    .execute(executor)
    .await?;

    Ok(id)
}

async fn allocate<'e, E>(
    executor: E,
    entry: Uuid,
    sale: Uuid,
    amount: Decimal,
) -> Result<(), sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(entry)
    .bind(sale)
    .bind(amount)
    .execute(executor)
    .await?;

    Ok(())
}

// --- The read model -----------------------------------------------------

#[sqlx::test]
async fn an_account_without_entries_owes_nothing(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");

    assert_eq!(position.balance, Decimal::ZERO);
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, Decimal::ZERO);
    assert_eq!(position.adjustments, Decimal::ZERO);
    assert!(position.is_consistent());

    assert!(
        repository::outstanding_by_sale(&pool, customer)
            .await
            .expect("debts")
            .is_empty()
    );
}

#[sqlx::test]
async fn the_balance_is_the_signed_sum_of_the_entries(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(150.00)).await;

    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00)))
        .await
        .expect("charge");
    insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(150.00)))
        .await
        .expect("receipt");
    insert_entry(
        &pool,
        NewEntry::adjustment(customer, dec!(20.00), "opening"),
    )
    .await
    .expect("adjustment in");

    // An outflow, which the adjustment helper does not build.
    let outflow = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO customer_account_entries
            (id, customer_id, kind, amount, note, created_by)
        VALUES ($1, $2, 'adjustment_out', $3, 'correction', $4)
        "#,
    )
    .bind(outflow)
    .bind(customer)
    .bind(dec!(-5.00))
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await
    .expect("adjustment out");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");

    // 100 charged - 150 collected + 20 - 5
    assert_eq!(position.balance, dec!(-35.00));
    assert_eq!(position.unapplied_credit, dec!(150.00));
    assert_eq!(position.outstanding_total, dec!(100.00));
    assert_eq!(position.adjustments, dec!(15.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn allocating_a_receipt_lowers_the_outstanding_and_the_usable_credit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(150.00)).await;

    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00)))
        .await
        .expect("charge");
    let receipt = insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(150.00)))
        .await
        .expect("receipt");

    allocate(&pool, receipt, sale, dec!(60.00))
        .await
        .expect("allocation");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");

    // The receipt already moved the balance when it arrived; allocating it says
    // which sale the money paid, and only that changes.
    assert_eq!(position.balance, dec!(-50.00));
    assert_eq!(position.unapplied_credit, dec!(90.00));
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());

    let debts = repository::outstanding_by_sale(&pool, customer)
        .await
        .expect("debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].charged, dec!(100.00));
    assert_eq!(debts[0].allocated, dec!(60.00));
    assert_eq!(debts[0].outstanding, dec!(40.00));
}

#[sqlx::test]
async fn a_fully_paid_sale_leaves_the_debt_list(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(40.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(40.00)).await;

    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(40.00)))
        .await
        .expect("charge");
    let receipt = insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(40.00)))
        .await
        .expect("receipt");
    allocate(&pool, receipt, sale, dec!(40.00))
        .await
        .expect("allocation");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");

    assert_eq!(position.balance, Decimal::ZERO);
    assert_eq!(position.outstanding_total, Decimal::ZERO);
    assert!(position.is_consistent());
    assert!(
        repository::outstanding_by_sale(&pool, customer)
            .await
            .expect("debts")
            .is_empty()
    );
}

// --- The table contract -------------------------------------------------

#[sqlx::test]
async fn an_entry_cannot_be_stored_with_the_sign_its_kind_forbids(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(50.00)).await;

    // A receipt stored positive would add debt where it removes it.
    let receipt_positive = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "receipt",
            amount: dec!(50.00),
            sale: None,
            movement: Some(movement),
            note: None,
        },
    )
    .await;
    assert!(receipt_positive.is_err(), "a positive receipt was stored");

    let charge_negative = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "sale_charge",
            amount: dec!(-100.00),
            sale: Some(sale),
            movement: None,
            note: None,
        },
    )
    .await;
    assert!(charge_negative.is_err(), "a negative charge was stored");

    let zero = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "adjustment_in",
            amount: Decimal::ZERO,
            sale: None,
            movement: None,
            note: Some("nothing"),
        },
    )
    .await;
    assert!(zero.is_err(), "a zero entry was stored");
}

#[sqlx::test]
async fn an_entry_must_name_the_document_its_kind_needs(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(50.00)).await;

    // A charge without its sale is debt from nowhere.
    let charge_without_sale = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "sale_charge",
            amount: dec!(100.00),
            sale: None,
            movement: None,
            note: None,
        },
    )
    .await;
    assert!(
        charge_without_sale.is_err(),
        "a charge without a sale was stored"
    );

    // Debt must never claim money no wallet saw.
    let receipt_without_movement = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "receipt",
            amount: dec!(-50.00),
            sale: None,
            movement: None,
            note: None,
        },
    )
    .await;
    assert!(
        receipt_without_movement.is_err(),
        "a receipt without a movement was stored"
    );

    // An adjustment without a reason is untraceable money.
    let adjustment_without_note = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "adjustment_in",
            amount: dec!(10.00),
            sale: None,
            movement: None,
            note: None,
        },
    )
    .await;
    assert!(
        adjustment_without_note.is_err(),
        "an adjustment without a reason was stored"
    );

    // A charge cannot also claim a movement: it never moved money.
    let charge_with_movement = insert_entry(
        &pool,
        NewEntry {
            customer,
            kind: "sale_charge",
            amount: dec!(100.00),
            sale: Some(sale),
            movement: Some(movement),
            note: None,
        },
    )
    .await;
    assert!(
        charge_with_movement.is_err(),
        "a charge pointing at a movement was stored"
    );
}

#[sqlx::test]
async fn the_ledger_cannot_be_rewritten(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let entry = insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00)))
        .await
        .expect("charge");

    let update = sqlx::query("UPDATE customer_account_entries SET amount = $2 WHERE id = $1")
        .bind(entry)
        .bind(dec!(10.00))
        .execute(&pool)
        .await;
    assert!(update.is_err(), "an entry was updated");

    let delete = sqlx::query("DELETE FROM customer_account_entries WHERE id = $1")
        .bind(entry)
        .execute(&pool)
        .await;
    assert!(delete.is_err(), "an entry was deleted");

    let amount: Decimal =
        sqlx::query_scalar("SELECT amount FROM customer_account_entries WHERE id = $1")
            .bind(entry)
            .fetch_one(&pool)
            .await
            .expect("row still there");
    assert_eq!(amount, dec!(100.00));
}

#[sqlx::test]
async fn a_sale_is_charged_to_the_account_once(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00)))
        .await
        .expect("first charge");

    let second = insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00))).await;
    assert!(second.is_err(), "the same sale was charged twice");
}

#[sqlx::test]
async fn a_wallet_movement_backs_at_most_one_entry(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let movement = seed_account_payment(&pool, customer, dec!(100.00)).await;

    insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(40.00)))
        .await
        .expect("first receipt");

    let second = insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(40.00))).await;
    assert!(second.is_err(), "one movement backed two entries");
}

#[sqlx::test]
async fn only_a_receipt_can_be_allocated_to_a_sale(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let charge = insert_entry(&pool, NewEntry::charge(customer, sale, dec!(100.00)))
        .await
        .expect("charge");

    let result = allocate(&pool, charge, sale, dec!(10.00)).await;
    assert!(result.is_err(), "a charge was allocated");
}

#[sqlx::test]
async fn an_allocation_cannot_exceed_its_receipt(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let first = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let second = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(50.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let receipt = insert_entry(&mut *tx, NewEntry::receipt(customer, movement, dec!(50.00)))
        .await
        .expect("receipt");

    // Each row fits on its own; together they do not. Only the deferred
    // constraint sees that, and only at commit.
    allocate(&mut *tx, receipt, first, dec!(30.00))
        .await
        .expect("first allocation");
    allocate(&mut *tx, receipt, second, dec!(30.00))
        .await
        .expect("second allocation");

    let commit = tx.commit().await;
    assert!(commit.is_err(), "a receipt paid more than it collected");
}

#[sqlx::test]
async fn an_allocation_cannot_pay_another_customers_sale(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let beto = seed_customer(&pool, "Beto").await;
    let beto_sale = seed_completed_sale(&pool, beto, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, ana, dec!(100.00)).await;

    let receipt = insert_entry(&pool, NewEntry::receipt(ana, movement, dec!(100.00)))
        .await
        .expect("receipt");

    let result = allocate(&pool, receipt, beto_sale, dec!(50.00)).await;
    assert!(result.is_err(), "a customer paid somebody else's sale");
}

#[sqlx::test]
async fn an_allocation_cannot_pay_a_sale_that_is_not_completed(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let draft = seed_draft_sale(&pool).await;
    let movement = seed_account_payment(&pool, customer, dec!(100.00)).await;

    let receipt = insert_entry(&pool, NewEntry::receipt(customer, movement, dec!(100.00)))
        .await
        .expect("receipt");

    let result = allocate(&pool, receipt, draft, dec!(50.00)).await;
    assert!(result.is_err(), "a draft sale was paid from the account");
}

// --- The write side -----------------------------------------------------

/// A draft that already has a customer: the state the POS is in while the
/// cashier is still adding items. The shared `seed_draft_sale` cannot serve a
/// charge test — its sale has no customer, and a customerless sale is
/// rejected as somebody else's before its status is ever looked at.
async fn seed_customer_draft_sale(pool: &PgPool, customer_id: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sales (id, status, created_by, customer_id) VALUES ($1, 'draft', $2, $3)",
    )
    .bind(id)
    .bind(Uuid::now_v7())
    .bind(customer_id)
    .execute(pool)
    .await
    .expect("draft sale");
    id
}

#[sqlx::test]
async fn charging_a_sale_raises_the_debt(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(100.00));
    assert_eq!(position.outstanding_total, dec!(100.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_receipt_collects_money_and_pays_the_sale(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    let recorded = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[(sale, dec!(60.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    assert!(!recorded.replayed);
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(40.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());

    let debts = repository::outstanding_by_sale(&pool, customer)
        .await
        .expect("debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].allocated, dec!(60.00));
}

#[sqlx::test]
async fn a_receipt_can_leave_money_unapplied(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(150.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(150.00),
            allocations: &[(sale, dec!(100.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(-50.00));
    assert_eq!(position.unapplied_credit, dec!(50.00));
    assert_eq!(position.outstanding_total, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_replayed_receipt_returns_the_first_one(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let first_movement = seed_account_payment(&pool, customer, dec!(60.00)).await;
    let second_movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let first = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: first_movement,
            amount: dec!(60.00),
            allocations: &[],
            idempotency_key: Some("till-close"),
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("first receipt");
    tx.commit().await.expect("commit");

    // A different movement carrying the same key: the replay must be decided
    // by the key, not by anything about the money.
    let mut tx = pool.begin().await.expect("transaction");
    let second = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: second_movement,
            amount: dec!(60.00),
            allocations: &[],
            idempotency_key: Some("till-close"),
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("replay");
    tx.commit().await.expect("commit");

    assert!(second.replayed);
    assert_eq!(second.entry_id, first.entry_id);

    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'receipt'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("receipt count");
    assert_eq!(receipts, 1);

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(-60.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_receipt_cannot_allocate_more_than_the_sale_owes(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    let movement = seed_account_payment(&pool, customer, dec!(100.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(100.00),
            allocations: &[(sale, dec!(150.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::OutstandingExceeded { .. })
    ));
    tx.rollback().await.expect("rollback");

    // The rejection must leave nothing behind: the ledger is append-only, so
    // half a receipt could never be taken back.
    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'receipt'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("receipt count");
    assert_eq!(receipts, 0);

    let allocations: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM customer_account_allocations a
        JOIN customer_account_entries e ON e.id = a.entry_id
        WHERE e.customer_id = $1
        "#,
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("allocation count");
    assert_eq!(allocations, 0);
}

#[sqlx::test]
async fn a_receipt_cannot_allocate_more_than_it_collected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let first = seed_completed_sale(&pool, customer, dec!(200.00)).await;
    let second = seed_completed_sale(&pool, customer, dec!(200.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, first, dec!(200.00), Uuid::now_v7())
        .await
        .expect("first charge");
    repository::record_charge(&mut tx, customer, second, dec!(200.00), Uuid::now_v7())
        .await
        .expect("second charge");
    tx.commit().await.expect("commit");

    let movement = seed_account_payment(&pool, customer, dec!(100.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(100.00),
            allocations: &[(first, dec!(100.00)), (second, dec!(100.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::ReceiptOverAllocated { .. })
    ));
    tx.rollback().await.expect("rollback");

    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'receipt'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("receipt count");
    assert_eq!(receipts, 0);

    let allocations: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM customer_account_allocations a
        JOIN customer_account_entries e ON e.id = a.entry_id
        WHERE e.customer_id = $1
        "#,
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("allocation count");
    assert_eq!(allocations, 0);
}

#[sqlx::test]
async fn an_amount_with_three_decimals_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_charge(&mut tx, customer, sale, dec!(10.005), Uuid::now_v7()).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::TooManyDecimals { .. })
    ));
    tx.rollback().await.expect("rollback");

    // `numeric(14, 2)` would have rounded the third decimal away; the rejection
    // is the point.
    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
            .bind(customer)
            .fetch_one(&pool)
            .await
            .expect("entry count");
    assert_eq!(entries, 0);
}

#[sqlx::test]
async fn a_charge_on_another_customers_sale_is_rejected(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let beto = seed_customer(&pool, "Beto").await;
    let beto_sale = seed_completed_sale(&pool, beto, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_charge(&mut tx, ana, beto_sale, dec!(100.00), Uuid::now_v7()).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::SaleOfAnotherCustomer)
    ));
    tx.rollback().await.expect("rollback");
}

#[sqlx::test]
async fn a_charge_on_a_draft_sale_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let draft = seed_customer_draft_sale(&pool, customer).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_charge(&mut tx, customer, draft, dec!(100.00), Uuid::now_v7()).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::SaleNotChargeable)
    ));
    tx.rollback().await.expect("rollback");
}

#[sqlx::test]
async fn the_same_sale_cannot_be_charged_twice(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("first charge");
    tx.commit().await.expect("commit");

    // The typed error, not a raw unique-index violation leaking out.
    let mut tx = pool.begin().await.expect("transaction");
    let second =
        repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7()).await;
    assert!(matches!(second, Err(repository::RepoError::AlreadyCharged)));
    tx.rollback().await.expect("rollback");

    let charges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE sale_id = $1 AND kind = 'sale_charge'",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("charge count");
    assert_eq!(charges, 1);
}

/// One collection attempt in its own transaction. The timeout is what turns a
/// lock that never comes into a failed test instead of a process that hangs.
async fn collect_within_ten_seconds(
    pool: PgPool,
    customer: Uuid,
    sale: Uuid,
    movement: Uuid,
) -> Result<(), repository::RepoError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut tx = pool.begin().await.expect("transaction");
        let result = repository::record_receipt(
            &mut tx,
            repository::NewReceipt {
                customer_id: customer,
                wallet_movement_id: movement,
                amount: dec!(60.00),
                allocations: &[(sale, dec!(60.00))],
                idempotency_key: None,
                note: None,
                created_by: Uuid::now_v7(),
            },
        )
        .await;
        if result.is_ok() {
            tx.commit().await.expect("commit");
        } else {
            tx.rollback().await.expect("rollback");
        }
        result.map(|_| ())
    })
    .await
    .expect("receipt within 10 s")
}

#[sqlx::test]
async fn two_concurrent_receipts_cannot_over_collect(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    let movement_a = seed_account_payment(&pool, customer, dec!(60.00)).await;
    let movement_b = seed_account_payment(&pool, customer, dec!(60.00)).await;

    // Two connections, two transactions: only the advisory lock can serialize
    // them, and this test is the proof that it does.
    let pool_a = pool.clone();
    let first = tokio::spawn(async move {
        collect_within_ten_seconds(pool_a, customer, sale, movement_a).await
    });
    let pool_b = pool.clone();
    let second = tokio::spawn(async move {
        collect_within_ten_seconds(pool_b, customer, sale, movement_b).await
    });

    let first = first.await.expect("task a");
    let second = second.await.expect("task b");

    let collected = [first, second].iter().filter(|r| r.is_ok()).count();
    assert_eq!(collected, 1, "two concurrent receipts both collected");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(40.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());

    let debts = repository::outstanding_by_sale(&pool, customer)
        .await
        .expect("debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].allocated, dec!(60.00));
}

#[sqlx::test]
async fn a_receipt_cannot_disagree_with_its_wallet_movement(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[(sale, dec!(60.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;

    assert!(
        matches!(
            result,
            Err(repository::RepoError::ReceiptDoesNotMatchMovement { .. })
        ),
        "a receipt recorded money the movement did not carry"
    );
    tx.rollback().await.expect("rollback");

    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM customer_account_entries")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(entries, 0, "the rejected receipt left an entry behind");
}

// --- Credit notes -------------------------------------------------------

/// A note document the ledger can point at. The ledger only needs the note to
/// exist for this sale; how its total splits between debt and cash belongs to
/// the credit-note slice that writes it.
async fn seed_credit_note(pool: &PgPool, sale_id: Uuid, total: Decimal) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(sale_id)
    .bind(total)
    .bind("wrong price charged")
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .expect("credit note");
    id
}

#[sqlx::test]
async fn a_credit_note_cancels_debt_and_lowers_the_outstanding(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let note = seed_credit_note(&pool, sale, dec!(30.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_credit_note(&mut tx, customer, sale, note, dec!(30.00), Uuid::now_v7())
        .await
        .expect("credit note");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(70.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(70.00));
    assert!(position.is_consistent());

    let debts = repository::outstanding_by_sale(&pool, customer)
        .await
        .expect("debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].cancelled, dec!(30.00));
    assert_eq!(debts[0].outstanding, dec!(70.00));
}

#[sqlx::test]
async fn a_credit_note_cannot_cancel_more_than_the_sale_owes(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let note = seed_credit_note(&pool, sale, dec!(150.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    let result =
        repository::record_credit_note(&mut tx, customer, sale, note, dec!(150.00), Uuid::now_v7())
            .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::OutstandingExceeded {
            requested,
            outstanding
        }) if requested == dec!(150.00) && outstanding == dec!(100.00)
    ));
    tx.rollback().await.expect("rollback");

    // Cancelling debt that does not exist must leave no entry behind.
    let notes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_note'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("note count");
    assert_eq!(notes, 0);
}

#[sqlx::test]
async fn a_credit_note_cannot_cancel_another_customers_debt(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let beto = seed_customer(&pool, "Beto").await;
    let beto_sale = seed_completed_sale(&pool, beto, dec!(100.00)).await;
    let note = seed_credit_note(&pool, beto_sale, dec!(30.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_credit_note(&mut tx, ana, beto_sale, note, dec!(30.00), Uuid::now_v7())
            .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::SaleOfAnotherCustomer)
    ));
    tx.rollback().await.expect("rollback");
}

#[sqlx::test]
async fn a_credit_note_is_recorded_once(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let note = seed_credit_note(&pool, sale, dec!(30.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_credit_note(&mut tx, customer, sale, note, dec!(30.00), Uuid::now_v7())
        .await
        .expect("first record");
    tx.commit().await.expect("commit");

    // The replay asks for less than the sale still owes, so only the note's
    // own identity can refuse it.
    let mut tx = pool.begin().await.expect("transaction");
    let second =
        repository::record_credit_note(&mut tx, customer, sale, note, dec!(10.00), Uuid::now_v7())
            .await;
    assert!(matches!(
        second,
        Err(repository::RepoError::AlreadyRecorded)
    ));
    tx.rollback().await.expect("rollback");

    let notes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_note'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("note count");
    assert_eq!(notes, 1);
}

// --- Credit refunds -----------------------------------------------------

#[sqlx::test]
async fn a_credit_refund_returns_money_and_clears_the_credit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let collected = seed_account_payment(&pool, customer, dec!(150.00)).await;
    let refunded = seed_account_payment_refund(&pool, customer, dec!(50.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: collected,
            amount: dec!(150.00),
            allocations: &[(sale, dec!(100.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    repository::record_credit_refund(
        &mut tx,
        customer,
        refunded,
        dec!(50.00),
        Some("customer asked for the credit back"),
        Uuid::now_v7(),
    )
    .await
    .expect("refund");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, Decimal::ZERO);
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_credit_refund_cannot_exceed_the_usable_credit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment_refund(&pool, customer, dec!(10.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // The customer owes 100 and holds nothing: debt is not money the shop can
    // hand back.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_credit_refund(
        &mut tx,
        customer,
        movement,
        dec!(10.00),
        None,
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditRefundExceedsCredit {
            available,
            ..
        }) if available == Decimal::ZERO
    ));
    tx.rollback().await.expect("rollback");

    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_refund'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 0);
}

#[sqlx::test]
async fn a_credit_refund_is_recorded_once_per_movement(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let collected = seed_account_payment(&pool, customer, dec!(150.00)).await;
    let refunded = seed_account_payment_refund(&pool, customer, dec!(30.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: collected,
            amount: dec!(150.00),
            allocations: &[(sale, dec!(100.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    repository::record_credit_refund(
        &mut tx,
        customer,
        refunded,
        dec!(30.00),
        None,
        Uuid::now_v7(),
    )
    .await
    .expect("first refund");
    tx.commit().await.expect("commit");

    // The first refund consumed part of the credit, so affordability alone
    // could no longer tell this apart from a fresh refund: the movement is
    // what identifies the replay.
    let mut tx = pool.begin().await.expect("transaction");
    let second = repository::record_credit_refund(
        &mut tx,
        customer,
        refunded,
        dec!(30.00),
        None,
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        second,
        Err(repository::RepoError::AlreadyRecorded)
    ));
    tx.rollback().await.expect("rollback");

    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_refund'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 1);
}

// --- Adjustments --------------------------------------------------------

#[sqlx::test]
async fn an_opening_balance_can_be_loaded_with_an_adjustment(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(500.00),
        "opening balance carried over from the previous books",
        Uuid::now_v7(),
    )
    .await
    .expect("adjustment");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(500.00));
    assert_eq!(position.adjustments, dec!(500.00));
    assert_eq!(position.outstanding_total, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn an_adjustment_cannot_cross_zero(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(150.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // Taking out more than the customer owes would flip the balance negative
    // and invent credit no receipt ever backed.
    let mut tx = pool.begin().await.expect("transaction");
    let out = repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::AdjustmentOut,
        dec!(150.00),
        "correction",
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        out,
        Err(repository::RepoError::AdjustmentCrossesZero { balance })
            if balance == dec!(100.00)
    ));
    tx.rollback().await.expect("rollback");

    // And once the customer holds credit, no adjustment may add debt on top of
    // it: the balance would cross zero in the other direction.
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(150.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    tx.commit().await.expect("commit");

    let mut tx = pool.begin().await.expect("transaction");
    let into_debt = repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(10.00),
        "correction",
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        into_debt,
        Err(repository::RepoError::AdjustmentCrossesZero { balance })
            if balance == dec!(-50.00)
    ));
    tx.rollback().await.expect("rollback");

    // Both rejections left nothing: the account still says one charge and one
    // receipt, and no adjustment at all.
    let adjustments: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM customer_account_entries
        WHERE customer_id = $1 AND kind IN ('adjustment_in', 'adjustment_out')
        "#,
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("adjustment count");
    assert_eq!(adjustments, 0);
}

#[sqlx::test]
async fn an_adjustment_needs_a_reason(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;

    let mut tx = pool.begin().await.expect("transaction");
    let blank = repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(10.00),
        "   ",
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        blank,
        Err(repository::RepoError::AdjustmentNoteRequired)
    ));
    tx.rollback().await.expect("rollback");

    // A charge is never typed by hand: it belongs to the sale that produced it.
    let mut tx = pool.begin().await.expect("transaction");
    let not_manual = repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::SaleCharge,
        dec!(10.00),
        "reason",
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        not_manual,
        Err(repository::RepoError::NotAnAdjustment)
    ));
    tx.rollback().await.expect("rollback");

    let adjustments: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM customer_account_entries
        WHERE customer_id = $1 AND kind IN ('adjustment_in', 'adjustment_out')
        "#,
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("adjustment count");
    assert_eq!(adjustments, 0);
}

#[sqlx::test]
async fn refunded_credit_cannot_be_allocated_again(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(150.00)).await;

    // 150 collected and nothing allocated: the whole amount is usable credit.
    let collected = seed_account_payment(&pool, customer, dec!(150.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let receipt = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: collected,
            amount: dec!(150.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt")
    .entry_id;
    tx.commit().await.expect("commit");

    // 30 of that money is handed back, so only 120 may still pay a sale.
    let refunded = seed_account_payment_refund(&pool, customer, dec!(30.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_credit_refund(
        &mut tx,
        customer,
        refunded,
        dec!(30.00),
        None,
        Uuid::now_v7(),
    )
    .await
    .expect("refund");
    tx.commit().await.expect("commit");

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(150.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // Each individual ceiling passes: the receipt collected 150 and the sale
    // owes 150. Only the customer-level credit says 30 of it is already gone.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::allocate(&mut tx, receipt, sale, dec!(150.00)).await;
    assert!(
        matches!(
            result,
            Err(repository::RepoError::AllocationExceedsCredit { .. })
        ),
        "allocated money that had already been handed back"
    );
    tx.rollback().await.expect("rollback");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.unapplied_credit, dec!(120.00));
    assert!(position.is_consistent(), "the books stopped balancing");
}

// --- The review advisories -----------------------------------------------

/// A wallet movement that collected part of a sale at the till. The account's
/// charge may only name the part of the total this movement did not collect,
/// and the sale kinds are also what a receipt must never be backed by.
async fn seed_sale_payment(pool: &PgPool, sale_id: Uuid, amount: Decimal) -> Uuid {
    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    let movement = Uuid::now_v7();
    let suffix = Uuid::new_v4();

    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
        .bind(wallet)
        .bind(format!("Wallet {suffix}"))
        .execute(pool)
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
        .bind(method)
        .bind(format!("Method {suffix}"))
        .bind(wallet)
        .execute(pool)
        .await
        .expect("method");
    sqlx::query(
        r#"
        INSERT INTO wallet_movements (id, wallet_id, kind, amount, sale_id, payment_method_id)
        VALUES ($1, $2, 'sale_payment', $3, $4, $5)
        "#,
    )
    .bind(movement)
    .bind(wallet)
    .bind(amount)
    .bind(sale_id)
    .bind(method)
    .execute(pool)
    .await
    .expect("sale payment");

    movement
}

#[sqlx::test]
async fn a_charge_cannot_exceed_the_unpaid_part_of_the_sale(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    seed_sale_payment(&pool, sale, dec!(60.00)).await;

    // 60 of the 100 were collected at the till, so only 40 may go on the
    // account: charging the collected part again would bill the customer for
    // money they already handed over.
    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_charge(&mut tx, customer, sale, dec!(50.00), Uuid::now_v7()).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::ChargeExceedsSaleTotal { requested, available })
            if requested == dec!(50.00) && available == dec!(40.00)
    ));
    tx.rollback().await.expect("rollback");

    // The unpaid part is exactly what a charge may carry.
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(40.00), Uuid::now_v7())
        .await
        .expect("charge of the unpaid part");
    tx.commit().await.expect("commit");
}

#[sqlx::test]
async fn a_charge_cannot_exceed_a_sale_without_payments(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_charge(&mut tx, customer, sale, dec!(150.00), Uuid::now_v7()).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::ChargeExceedsSaleTotal { requested, available })
            if requested == dec!(150.00) && available == dec!(100.00)
    ));
    tx.rollback().await.expect("rollback");

    let charges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE sale_id = $1 AND kind = 'sale_charge'",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("charge count");
    assert_eq!(charges, 0);
}

#[sqlx::test]
async fn a_credit_note_must_belong_to_the_sale_it_cancels(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let charged = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let other = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, charged, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_charge(&mut tx, customer, other, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // Both sales owe enough, so the only thing this entry can be refused for
    // is the document: the note is about the first sale, not the second.
    let note = seed_credit_note(&pool, charged, dec!(30.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let result =
        repository::record_credit_note(&mut tx, customer, other, note, dec!(30.00), Uuid::now_v7())
            .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditNoteMismatch)
    ));
    tx.rollback().await.expect("rollback");

    let notes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_note'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("note count");
    assert_eq!(notes, 0);
}

#[sqlx::test]
async fn a_credit_note_cannot_cancel_more_than_the_note_carries(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let note = seed_credit_note(&pool, sale, dec!(20.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    let result =
        repository::record_credit_note(&mut tx, customer, sale, note, dec!(30.00), Uuid::now_v7())
            .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditNoteExceeded { requested, available })
            if requested == dec!(30.00) && available == dec!(20.00)
    ));
    tx.rollback().await.expect("rollback");

    let notes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_note'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("note count");
    assert_eq!(notes, 0);
}

#[sqlx::test]
async fn a_receipt_cannot_name_the_same_sale_twice(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            // Both rows fit every per-row ceiling; only the repetition is
            // wrong, and it must be refused before the entry is written.
            allocations: &[(sale, dec!(30.00)), (sale, dec!(30.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::DuplicateAllocation { sale_id }) if sale_id == sale
    ));
    tx.rollback().await.expect("rollback");

    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'receipt'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("receipt count");
    assert_eq!(receipts, 0, "the rejected receipt left an entry behind");
}

#[sqlx::test]
async fn a_receipt_cannot_be_backed_by_a_sale_payment(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_sale_payment(&pool, sale, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[(sale, dec!(60.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(repository::RepoError::MovementNotFromAnAccount { kind })
            if kind == "sale_payment"
    ));
    tx.rollback().await.expect("rollback");

    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
            .bind(customer)
            .fetch_one(&pool)
            .await
            .expect("entry count");
    assert_eq!(entries, 0, "the rejected receipt left an entry behind");
}

#[sqlx::test]
async fn a_receipt_backed_by_another_customers_movement_is_rejected(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let beto = seed_customer(&pool, "Beto").await;
    let movement = seed_account_payment(&pool, beto, dec!(60.00)).await;

    // The money exists, the amount agrees and the kind is not the till's own
    // accounting of a sale: only the customer binding can refuse this, and it
    // is the one a receipt cannot live without — collecting Beto's money
    // against Ana's debt would move money between two people's accounts.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(
        matches!(
            result,
            Err(repository::RepoError::MovementOfAnotherCustomer)
        ),
        "a receipt was backed by another customer's movement"
    );
    tx.rollback().await.expect("rollback");

    // The rejection must leave nothing behind: no entry for Ana, and Beto's
    // movement still backs nothing.
    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
            .bind(ana)
            .fetch_one(&pool)
            .await
            .expect("entry count");
    assert_eq!(entries, 0, "the rejected receipt left an entry behind");

    let backing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE wallet_movement_id = $1",
    )
    .bind(movement)
    .fetch_one(&pool)
    .await
    .expect("backing count");
    assert_eq!(backing, 0);
}

#[sqlx::test]
async fn allocating_a_receipt_succeeds_and_moves_the_debt(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    let receipt = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");

    // The success path `allocate` never had: money already collected, pointed
    // at the sale it pays after the fact.
    repository::allocate(&mut tx, receipt.entry_id, sale, dec!(60.00))
        .await
        .expect("allocation");
    tx.commit().await.expect("commit");

    let allocations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_allocations WHERE entry_id = $1 AND sale_id = $2",
    )
    .bind(receipt.entry_id)
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("allocation count");
    assert_eq!(allocations, 1);

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(40.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn the_outstanding_total_agrees_with_the_sale_list(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let first = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let second = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, first, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_charge(&mut tx, customer, second, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(60.00),
            allocations: &[(first, dec!(40.00))],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    let debts = repository::outstanding_by_sale(&pool, customer)
        .await
        .expect("debts");

    // One statement, one snapshot: the scalar the position carries is the sum
    // of the rows the statement's own definition lists, so the two surfaces
    // cannot disagree.
    let listed: Decimal = debts.iter().map(|debt| debt.outstanding).sum();
    assert_eq!(position.outstanding_total, listed);
    assert_eq!(position.outstanding_total, dec!(160.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn an_allocation_beyond_the_sale_charge_is_rejected_by_the_engine(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    let movement = seed_account_payment(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    let receipt = insert_entry(&mut *tx, NewEntry::receipt(customer, movement, dec!(60.00)))
        .await
        .expect("receipt");

    // The sale carries no charge, so the allocation claims a debt that was
    // never on the account. The per-row checks pass — the receipt exists, the
    // sale is completed and belongs to the customer — and only the deferred
    // engine ceiling can refuse it, at commit.
    allocate(&mut *tx, receipt, sale, dec!(50.00))
        .await
        .expect("insert accepted before the constraint fires");

    let commit = tx.commit().await;
    assert!(
        commit.is_err(),
        "an allocation paid a sale that was never charged"
    );
}

// --- Credit limits ------------------------------------------------------

/// Set the customer's credit limit straight on the row. These tests exercise
/// the gate, not the registry route that writes the field, so the limit is
/// fixture state like the customer itself.
async fn set_credit_limit(pool: &PgPool, customer: Uuid, limit: Option<Decimal>) {
    sqlx::query("UPDATE customers SET credit_limit = $2 WHERE id = $1")
        .bind(customer)
        .bind(limit)
        .execute(pool)
        .await
        .expect("credit limit");
}

/// How many entries one customer's ledger carries: the "nothing was written"
/// assertion of every rejected gate below.
async fn entry_count(pool: &PgPool, customer: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
        .bind(customer)
        .fetch_one(pool)
        .await
        .expect("entry count")
}

#[sqlx::test]
async fn a_charge_beyond_the_credit_limit_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    set_credit_limit(&pool, customer, Some(dec!(100.00))).await;
    let sale = seed_completed_sale(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(60.00), Uuid::now_v7())
        .await
        .expect("opening charge");
    tx.commit().await.expect("commit");

    // The customer owes 60 of a 100 limit, so 50 more does not fit: the
    // exposure after the sale would be 110, and the message names the 40
    // the customer still has available.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::authorize_credit(&mut tx, customer, dec!(50.00)).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditLimitExceeded { requested, available })
            if requested == dec!(110.00) && available == dec!(40.00)
    ));
    tx.rollback().await.expect("rollback");

    // A gate authorizes; it never writes. The ledger still says one charge.
    assert_eq!(entry_count(&pool, customer).await, 1);
    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(60.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_customer_without_a_limit_can_owe_any_amount(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(5000.00)).await;

    // NULL is the default and means no limit: the gate is the activity check
    // alone, and the account takes whatever the completion asks for.
    let mut tx = pool.begin().await.expect("transaction");
    repository::authorize_credit(&mut tx, customer, dec!(5000.00))
        .await
        .expect("no limit to exceed");
    repository::record_charge(&mut tx, customer, sale, dec!(5000.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(5000.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_zero_limit_is_cash_only(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    set_credit_limit(&pool, customer, Some(Decimal::ZERO)).await;

    // Zero is a limit, not the absence of one: the customer may buy, but
    // nothing on account.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::authorize_credit(&mut tx, customer, dec!(1.00)).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditLimitExceeded { available, .. })
            if available == Decimal::ZERO
    ));
    tx.rollback().await.expect("rollback");
    assert_eq!(entry_count(&pool, customer).await, 0);

    // A customer holding money in the shop's favour is different: the charge
    // first spends that money, and only what it leaves over is credit. A
    // 30 charge against 50 held never asks for credit at all.
    let movement = seed_account_payment(&pool, customer, dec!(50.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: movement,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    repository::authorize_credit(&mut tx, customer, dec!(30.00))
        .await
        .expect("money already in the customer's favour is not credit");
    tx.commit().await.expect("commit");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(-50.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn the_limit_counts_what_the_customer_already_owes(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    set_credit_limit(&pool, customer, Some(dec!(100.00))).await;
    let sale = seed_completed_sale(&pool, customer, dec!(60.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(60.00), Uuid::now_v7())
        .await
        .expect("opening charge");
    tx.commit().await.expect("commit");

    // The ceiling is on the exposure, not on the single sale: 40 still fits,
    // 41 does not, and the difference is exactly the debt already carried.
    let mut tx = pool.begin().await.expect("transaction");
    repository::authorize_credit(&mut tx, customer, dec!(40.00))
        .await
        .expect("40 fits the remaining limit");
    let result = repository::authorize_credit(&mut tx, customer, dec!(41.00)).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CreditLimitExceeded { requested, available })
            if requested == dec!(101.00) && available == dec!(40.00)
    ));
    tx.rollback().await.expect("rollback");

    assert_eq!(entry_count(&pool, customer).await, 1);
}

#[sqlx::test]
async fn an_inactive_customer_cannot_be_charged(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    sqlx::query("UPDATE customers SET is_active = false WHERE id = $1")
        .bind(customer)
        .execute(&pool)
        .await
        .expect("deactivate");

    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::authorize_credit(&mut tx, customer, dec!(50.00)).await;
    assert!(matches!(
        result,
        Err(repository::RepoError::CustomerInactive)
    ));
    tx.rollback().await.expect("rollback");

    assert_eq!(entry_count(&pool, customer).await, 0);
}

#[sqlx::test]
async fn lowering_the_limit_below_the_balance_blocks_only_new_credit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    set_credit_limit(&pool, customer, Some(dec!(100.00))).await;

    // The debt is loaded the way the books load it: an opening balance, not
    // a sale, because a charge this size would itself be refused by the gate
    // the test is about to exercise.
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_adjustment(
        &mut tx,
        customer,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(120.00),
        "opening balance from the previous books",
        Uuid::now_v7(),
    )
    .await
    .expect("opening balance");
    tx.commit().await.expect("commit");

    // The limit is now below the balance, and that is allowed: the operator
    // decided no NEW credit leaves the shop. The existing 120 is untouched —
    // lowering a number on the registry rewrites nothing on the ledger.
    let mut tx = pool.begin().await.expect("transaction");
    let result = repository::authorize_credit(&mut tx, customer, dec!(1.00)).await;
    assert!(
        matches!(
            result,
            Err(repository::RepoError::CreditLimitExceeded { available, .. })
                if available == Decimal::ZERO
        ),
        "a new charge is blocked"
    );
    tx.rollback().await.expect("rollback");

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(120.00), "the existing debt stays");
    assert_eq!(entry_count(&pool, customer).await, 1);
    assert!(position.is_consistent());
}

/// Authorizing nothing is always allowed. A customer whose limit was lowered
/// below what they already owe is over it by definition, and a no-op charge
/// must not answer "refused": that sentence says nothing about this charge.
#[sqlx::test]
async fn authorizing_nothing_is_allowed_even_over_the_limit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(120.00)).await;
    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(120.00)))
        .await
        .expect("charge");

    sqlx::query("UPDATE customers SET credit_limit = 100.00 WHERE id = $1")
        .bind(customer)
        .execute(&pool)
        .await
        .expect("limit");

    let mut tx = pool.begin().await.expect("transaction");
    repository::authorize_credit(&mut tx, customer, Decimal::ZERO)
        .await
        .expect("a zero charge needs no credit");

    let refused = repository::authorize_credit(&mut tx, customer, dec!(1.00)).await;
    assert!(
        matches!(
            refused,
            Err(repository::RepoError::CreditLimitExceeded { .. })
        ),
        "the limit stopped applying once the customer was already over it"
    );
    tx.rollback().await.expect("rollback");
}

/// The refusal sentence is part of what a reviewer and an operator read. It
/// names the exposure the charge would produce, never the charge itself: the
/// value it carries includes the balance the customer already had.
/// Plain `#[test]`: formatting a sentence needs no database, and paying for one
/// would hide that this assertion is about wording alone.
#[test]
fn the_refusal_sentence_names_the_exposure_not_the_charge() {
    let sentence = repository::RepoError::CreditLimitExceeded {
        requested: dec!(121.00),
        available: Decimal::ZERO,
    }
    .to_string();

    assert!(sentence.contains("the account would reach"), "{sentence}");
    assert!(sentence.contains("is available"), "{sentence}");
    assert!(
        !sentence.contains("the charge of"),
        "the sentence calls the post-charge exposure a charge: {sentence}"
    );
}

/// A negative amount is out of domain and every money writer rejects it, so the
/// gate must not bless it as "nothing to authorize": only an exact zero is a
/// no-op.
#[sqlx::test]
async fn a_negative_amount_is_not_authorized_as_nothing(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(120.00)).await;
    insert_entry(&pool, NewEntry::charge(customer, sale, dec!(120.00)))
        .await
        .expect("charge");
    sqlx::query("UPDATE customers SET credit_limit = 100.00 WHERE id = $1")
        .bind(customer)
        .execute(&pool)
        .await
        .expect("limit");

    let mut tx = pool.begin().await.expect("transaction");
    let refused = repository::authorize_credit(&mut tx, customer, dec!(-1.00)).await;
    match refused {
        Err(repository::RepoError::CreditLimitExceeded {
            requested,
            available,
        }) => {
            // 120 already owed, minus the 1 the negative amount subtracts.
            assert_eq!(requested, dec!(119.00));
            // A 100 limit against 120 of exposure leaves nothing available.
            assert_eq!(available, Decimal::ZERO);
        }
        other => panic!("a negative amount slipped through the ceiling: {other:?}"),
    }
    tx.rollback().await.expect("rollback");
}

// --- Credit application --------------------------------------------------

/// The credit the account can spend is consumed oldest receipt first: a
/// sale that takes only part of the credit draws it all from the older
/// receipt, and the newer one keeps everything it collected.
#[sqlx::test]
async fn applying_credit_consumes_the_oldest_receipt_first(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;

    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(100.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // Two receipts in two committed transactions: their `created_at` values
    // order them, and only that ordering decides which one pays.
    let older_movement = seed_account_payment(&pool, customer, dec!(50.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let older = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: older_movement,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("older receipt")
    .entry_id;
    tx.commit().await.expect("commit");

    let newer_movement = seed_account_payment(&pool, customer, dec!(30.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    let newer = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: newer_movement,
            amount: dec!(30.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("newer receipt")
    .entry_id;
    tx.commit().await.expect("commit");

    // The sale takes only 20 of the 80 the customer holds.
    let mut tx = pool.begin().await.expect("transaction");
    let applied = repository::apply_available_credit(&mut tx, customer, sale, dec!(20.00))
        .await
        .expect("credit applied");
    tx.commit().await.expect("commit");

    assert_eq!(applied, dec!(20.00));

    // The one allocation row names the older receipt; the newer one was
    // never touched.
    let (entry_id, amount): (Uuid, Decimal) = sqlx::query_as(
        "SELECT entry_id, amount FROM customer_account_allocations WHERE sale_id = $1",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("allocation");
    assert_eq!((entry_id, amount), (older, dec!(20.00)));
    let newer_allocations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM customer_account_allocations WHERE entry_id = $1")
            .bind(newer)
            .fetch_one(&pool)
            .await
            .expect("allocation count");
    assert_eq!(newer_allocations, 0, "the newer receipt paid first");

    // 80 collected, 20 spent on the sale: 60 stays usable, the sale keeps
    // owing 80, and the books balance.
    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.unapplied_credit, dec!(60.00));
    assert_eq!(position.outstanding_total, dec!(80.00));
    assert_eq!(position.balance, dec!(20.00));
    assert!(position.is_consistent());
}

/// The application never exceeds the usable credit, even when the receipts
/// carry more: money already handed back as a refund sits inside the receipt's
/// own remainder, and only the customer-level ceiling can refuse it.
#[sqlx::test]
async fn applying_credit_never_exceeds_the_usable_credit(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(200.00)).await;

    // 150 collected, then 50 handed back while the balance is still in the
    // customer's favour (a refund is only allowed against money actually
    // held): the receipt now carries 150 of unallocated money, but only 100
    // of it is the customer's to spend.
    let collected = seed_account_payment(&pool, customer, dec!(150.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: customer,
            wallet_movement_id: collected,
            amount: dec!(150.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await
    .expect("receipt");
    tx.commit().await.expect("commit");

    let refunded = seed_account_payment_refund(&pool, customer, dec!(50.00)).await;
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_credit_refund(
        &mut tx,
        customer,
        refunded,
        dec!(50.00),
        None,
        Uuid::now_v7(),
    )
    .await
    .expect("refund");
    tx.commit().await.expect("commit");

    // Only now does the debt exist, larger than the credit that can pay it.
    let mut tx = pool.begin().await.expect("transaction");
    repository::record_charge(&mut tx, customer, sale, dec!(200.00), Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");

    // `up_to` is the whole charge and the sale owes all of it; only the
    // usable credit can stop the application at 100.
    let mut tx = pool.begin().await.expect("transaction");
    let applied = repository::apply_available_credit(&mut tx, customer, sale, dec!(200.00))
        .await
        .expect("credit applied");
    tx.commit().await.expect("commit");

    assert_eq!(
        applied,
        dec!(100.00),
        "the application spent refunded money"
    );

    let position = repository::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(100.00));
    assert_eq!(position.balance, dec!(100.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn account_movements_cannot_back_entries_of_the_opposite_direction(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, 'Cash')")
        .bind(wallet)
        .execute(&pool)
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, 'Cash', $2)")
        .bind(method)
        .bind(wallet)
        .execute(&pool)
        .await
        .expect("method");

    let inflow = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment', $3, $4, $5)",
    )
    .bind(inflow)
    .bind(wallet)
    .bind(dec!(50.00))
    .bind(method)
    .bind(ana)
    .execute(&pool)
    .await
    .expect("inflow");

    let outflow = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment_refund', $3, $4, $5)",
    )
    .bind(outflow)
    .bind(wallet)
    .bind(dec!(50.00))
    .bind(method)
    .bind(ana)
    .execute(&pool)
    .await
    .expect("outflow");

    // A receipt cannot be backed by an `account_payment_refund`.
    let mut tx = pool.begin().await.expect("tx");
    let receipt_err = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: outflow,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: Uuid::now_v7(),
        },
    )
    .await;
    assert!(matches!(
        receipt_err,
        Err(repository::RepoError::MovementNotFromAnAccount { ref kind })
            if kind == "account_payment_refund"
    ));
    tx.rollback().await.expect("rollback");

    // A credit refund cannot be backed by an `account_payment`.
    let mut tx = pool.begin().await.expect("tx");
    let refund_err = repository::record_credit_refund(
        &mut tx,
        ana,
        inflow,
        dec!(50.00),
        Some("wrong direction"),
        Uuid::now_v7(),
    )
    .await;
    assert!(matches!(
        refund_err,
        Err(repository::RepoError::MovementNotFromAnAccount { ref kind })
            if kind == "account_payment"
    ));
    tx.rollback().await.expect("rollback");
}

#[sqlx::test]
async fn adjustment_in_respects_non_credit_drift_across_mixed_states(pool: PgPool) {
    let actor = Uuid::now_v7();

    // Case A: O > 0, U > 0, A < 0, B < 0 with O + A < 0.
    // Sales 100 + 50, adjustment_out 140, receipt 100 allocated to sale 1,
    // unallocated receipt 20 -> B = -110, U = 20, O = 50, A = -140, drift = 90.
    let ana = seed_customer(&pool, "Ana Mixed").await;
    let s1 = seed_completed_sale(&pool, ana, dec!(100.00)).await;
    let s2 = seed_completed_sale(&pool, ana, dec!(50.00)).await;
    let m1 = seed_account_payment(&pool, ana, dec!(100.00)).await;
    let m2 = seed_account_payment(&pool, ana, dec!(20.00)).await;

    let mut tx = pool.begin().await.unwrap();
    repository::record_charge(&mut tx, ana, s1, dec!(100.00), actor)
        .await
        .unwrap();
    repository::record_charge(&mut tx, ana, s2, dec!(50.00), actor)
        .await
        .unwrap();
    repository::record_adjustment(
        &mut tx,
        ana,
        CustomerAccountEntryKind::AdjustmentOut,
        dec!(140.00),
        "over-adjustment",
        actor,
    )
    .await
    .unwrap();
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: m1,
            amount: dec!(100.00),
            allocations: &[(s1, dec!(100.00))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: m2,
            amount: dec!(20.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 90.01 exceeds non-credit drift (90.00) -> rejected.
    let mut tx = pool.begin().await.unwrap();
    let err = repository::record_adjustment(
        &mut tx,
        ana,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(90.01),
        "exceeds drift",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        repository::RepoError::AdjustmentCrossesZero { balance } if balance == dec!(-110.00)
    ));
    tx.rollback().await.unwrap();

    // Exact 90.00 drift succeeds -> B = -20, U = 20, O = 50, A = -50.
    let mut tx = pool.begin().await.unwrap();
    repository::record_adjustment(
        &mut tx,
        ana,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(90.00),
        "reconcile drift",
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = repository::account_position(&pool, ana).await.unwrap();
    assert_eq!(pos.balance, dec!(-20.00));
    assert_eq!(pos.unapplied_credit, dec!(20.00));
    assert_eq!(pos.outstanding_total, dec!(50.00));
    assert!(pos.is_consistent());

    // Refunding the full 20.00 real credit remains possible (0 cents buried).
    let wallet: Uuid = sqlx::query_scalar("SELECT wallet_id FROM wallet_movements WHERE id = $1")
        .bind(m2)
        .fetch_one(&pool)
        .await
        .unwrap();
    let method: Uuid =
        sqlx::query_scalar("SELECT payment_method_id FROM wallet_movements WHERE id = $1")
            .bind(m2)
            .fetch_one(&pool)
            .await
            .unwrap();
    let refund_mv = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment_refund', $3, $4, $5)",
    )
    .bind(refund_mv)
    .bind(wallet)
    .bind(dec!(20.00))
    .bind(method)
    .bind(ana)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    repository::record_credit_refund(&mut tx, ana, refund_mv, dec!(20.00), Some("refund"), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let pos = repository::account_position(&pool, ana).await.unwrap();
    assert_eq!(pos.balance, Decimal::ZERO);
    assert_eq!(pos.unapplied_credit, Decimal::ZERO);
    assert!(pos.is_consistent());

    // Case B: O > 0, U > 0, A < 0, B < 0 with O + A >= 0.
    // Sale 100, adjustment_out 10, unallocated receipt 120 -> B = -30, U = 120, O = 100.
    // non_credit_drift = (30 - 120).max(0) = 0 -> rejected until allocated.
    let beto = seed_customer(&pool, "Beto Mixed").await;
    let bs1 = seed_completed_sale(&pool, beto, dec!(100.00)).await;
    let bm1 = seed_account_payment(&pool, beto, dec!(120.00)).await;

    let mut tx = pool.begin().await.unwrap();
    repository::record_charge(&mut tx, beto, bs1, dec!(100.00), actor)
        .await
        .unwrap();
    repository::record_adjustment(
        &mut tx,
        beto,
        CustomerAccountEntryKind::AdjustmentOut,
        dec!(10.00),
        "discount",
        actor,
    )
    .await
    .unwrap();
    let receipt = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: beto,
            wallet_movement_id: bm1,
            amount: dec!(120.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = repository::record_adjustment(
        &mut tx,
        beto,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(0.01),
        "cannot shrink refundable -B",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        repository::RepoError::AdjustmentCrossesZero { balance } if balance == dec!(-30.00)
    ));
    tx.rollback().await.unwrap();

    // Allocate 100 to the sale -> B = -30, U = 20, O = 0, drift = 10 -> unlocks reconciliation.
    let mut tx = pool.begin().await.unwrap();
    repository::allocate(&mut tx, receipt.entry_id, bs1, dec!(100.00))
        .await
        .unwrap();
    repository::record_adjustment(
        &mut tx,
        beto,
        CustomerAccountEntryKind::AdjustmentIn,
        dec!(10.00),
        "reconcile drift after allocation",
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = repository::account_position(&pool, beto).await.unwrap();
    assert_eq!(pos.balance, dec!(-20.00));
    assert_eq!(pos.unapplied_credit, dec!(20.00));
    assert_eq!(pos.outstanding_total, Decimal::ZERO);
    assert!(pos.is_consistent());
}

#[sqlx::test]
async fn credit_refund_attribution_blocks_manual_allocate_prefers_same_wallet_and_enforces_triggers(
    pool: PgPool,
) {
    let actor = Uuid::now_v7();
    let ana = seed_customer(&pool, "Ana Refund Attribution").await;

    let wallet_a = Uuid::now_v7();
    let method_a = Uuid::now_v7();
    let wallet_b = Uuid::now_v7();
    let method_b = Uuid::now_v7();
    for (w, w_name, m, m_name) in [
        (wallet_a, "Cash A", method_a, "Cash Method A"),
        (wallet_b, "Card B", method_b, "Card Method B"),
    ] {
        sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
            .bind(w)
            .bind(w_name)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
            .bind(m)
            .bind(m_name)
            .bind(w)
            .execute(&pool)
            .await
            .unwrap();
    }

    let insert_mv = |wallet: Uuid, method: Uuid, kind: &'static str, amount: Decimal| {
        let pool = pool.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
                VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)
                "#,
            )
            .bind(id)
            .bind(wallet)
            .bind(kind)
            .bind(amount)
            .bind(method)
            .bind(ana)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };

    // Case 1: Receipt 1 ($100, Wallet A) refunded in full, then Receipt 2 ($50, Wallet B).
    // Manual allocate() on refunded Receipt 1 MUST fail with available = 0 even though
    // account-level unapplied_credit is 50.
    let mv_r1 = insert_mv(wallet_a, method_a, "account_payment", dec!(100.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let r1 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r1,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mv_ref1 = insert_mv(wallet_a, method_a, "account_payment_refund", dec!(100.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let ref1 = repository::record_credit_refund(
        &mut tx,
        ana,
        mv_ref1,
        dec!(100.00),
        Some("full refund"),
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mv_r2 = insert_mv(wallet_b, method_b, "account_payment", dec!(50.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let r2 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r2,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let sale1 = seed_completed_sale(&pool, ana, dec!(50.00)).await;
    let mut tx = pool.begin().await.unwrap();
    repository::record_charge(&mut tx, ana, sale1, dec!(50.00), actor)
        .await
        .unwrap();
    let err = repository::allocate(&mut tx, r1, sale1, dec!(50.00))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        repository::RepoError::AllocationExceedsCredit {
            requested,
            available
        } if requested == dec!(50.00) && available == Decimal::ZERO
    ));

    // Direct SQL insert bypassing repository must also be rejected by Postgres trigger.
    let raw_res = sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(r1)
    .bind(sale1)
    .bind(dec!(50.00))
    .execute(&mut *tx)
    .await;
    assert!(raw_res.is_ok(), "deferred trigger runs at commit");
    let commit_err = tx.commit().await.unwrap_err();
    assert!(
        commit_err
            .to_string()
            .contains("allocations (50.00) plus refunds (100.00) exceed"),
        "expected receipt ceiling trigger error, got: {commit_err}"
    );

    // Append-only trigger on customer_account_credit_refund_allocations.
    let del_err = sqlx::query(
        "DELETE FROM customer_account_credit_refund_allocations WHERE refund_entry_id = $1",
    )
    .bind(ref1)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        del_err.to_string().contains("append-only"),
        "expected append-only error, got: {del_err}"
    );

    // Unattributed credit_refund raw SQL insert must fail at COMMIT.
    let mv_orphan = insert_mv(wallet_b, method_b, "account_payment_refund", dec!(10.00)).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(ana)
    .bind(dec!(10.00))
    .bind(mv_orphan)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .unwrap();
    let orphan_err = tx.commit().await.unwrap_err();
    assert!(
        orphan_err.to_string().contains("must be fully attributed"),
        "expected completeness trigger error, got: {orphan_err}"
    );

    // Allocating from Receipt 2 succeeds and settles sale1.
    let mut tx = pool.begin().await.unwrap();
    repository::record_charge(&mut tx, ana, sale1, dec!(50.00), actor)
        .await
        .unwrap();
    repository::allocate(&mut tx, r2, sale1, dec!(50.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Case 2: Both Receipt 3 ($100, Wallet A) and Receipt 4 ($50, Wallet B) are open.
    // Refunding $50 via Wallet B prefers Receipt 4 (same wallet), leaving Receipt 3 ($100)
    // fully intact; then refunding $40 via Wallet A leaves $60 on Receipt 3.
    let mv_r3 = insert_mv(wallet_a, method_a, "account_payment", dec!(100.00)).await;
    let mv_r4 = insert_mv(wallet_b, method_b, "account_payment", dec!(50.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let r3 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r3,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    let r4 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r4,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mv_ref_b = insert_mv(wallet_b, method_b, "account_payment_refund", dec!(50.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let ref_b = repository::record_credit_refund(
        &mut tx,
        ana,
        mv_ref_b,
        dec!(50.00),
        Some("refund card"),
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let consumed_by_b: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT receipt_entry_id, amount FROM customer_account_credit_refund_allocations WHERE refund_entry_id = $1",
    )
    .bind(ref_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(consumed_by_b, vec![(r4, dec!(50.00))]);

    let mv_ref_a = insert_mv(wallet_a, method_a, "account_payment_refund", dec!(40.00)).await;
    let mut tx = pool.begin().await.unwrap();
    repository::record_credit_refund(
        &mut tx,
        ana,
        mv_ref_a,
        dec!(40.00),
        Some("partial cash refund"),
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let sale2 = seed_completed_sale(&pool, ana, dec!(80.00)).await;
    let mut tx = pool.begin().await.unwrap();
    repository::record_charge(&mut tx, ana, sale2, dec!(80.00), actor)
        .await
        .unwrap();
    let applied = repository::apply_available_credit(&mut tx, ana, sale2, dec!(80.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(applied, dec!(60.00));

    let alloc_entries: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry_id, amount FROM customer_account_allocations WHERE sale_id = $1",
    )
    .bind(sale2)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(alloc_entries, vec![(r3, dec!(60.00))]);
}

#[sqlx::test]
async fn credit_refund_cross_wallet_split_idempotency_and_negative_triggers(pool: PgPool) {
    let actor = Uuid::now_v7();
    let ana = seed_customer(&pool, "Ana Split").await;
    let beto = seed_customer(&pool, "Beto Other").await;

    let wallet_a = Uuid::now_v7();
    let method_a = Uuid::now_v7();
    let wallet_b = Uuid::now_v7();
    let method_b = Uuid::now_v7();
    for (w, w_name, m, m_name) in [
        (wallet_a, "Cash Split A", method_a, "Cash Split Method A"),
        (wallet_b, "Card Split B", method_b, "Card Split Method B"),
    ] {
        sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
            .bind(w)
            .bind(w_name)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
            .bind(m)
            .bind(m_name)
            .bind(w)
            .execute(&pool)
            .await
            .unwrap();
    }

    let insert_mv = |customer: Uuid,
                     wallet: Uuid,
                     method: Uuid,
                     kind: &'static str,
                     amount: Decimal| {
        let pool = pool.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
                VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)
                "#,
            )
            .bind(id)
            .bind(wallet)
            .bind(kind)
            .bind(amount)
            .bind(method)
            .bind(customer)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };

    // R1 ($40, Wallet A), R2 ($30, Wallet B), R3 ($50, Wallet A).
    let mv_r1 = insert_mv(ana, wallet_a, method_a, "account_payment", dec!(40.00)).await;
    let mv_r2 = insert_mv(ana, wallet_b, method_b, "account_payment", dec!(30.00)).await;
    let mv_r3 = insert_mv(ana, wallet_a, method_a, "account_payment", dec!(50.00)).await;
    let mv_beto = insert_mv(beto, wallet_a, method_a, "account_payment", dec!(20.00)).await;

    let mut tx = pool.begin().await.unwrap();
    let r1 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r1,
            amount: dec!(40.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    let r2 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r2,
            amount: dec!(30.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    let r3 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r3,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    let beto_r = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: beto,
            wallet_movement_id: mv_beto,
            amount: dec!(20.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    // Refund $100 via Wallet A: must consume R1 ($40, Wallet A), R3 ($50, Wallet A),
    // then spill $10 into R2 (Wallet B), leaving $20 on R2.
    let refund_out = organox::customer_accounts::service::refund_credit(
        &pool,
        ana,
        organox::customer_accounts::service::RefundCreditInput {
            payment_method_id: method_a,
            amount: dec!(100.00),
            note: "split refund",
            idempotency_key: Some("split-ref-key"),
            actor,
        },
    )
    .await
    .unwrap();
    assert!(!refund_out.replayed);

    let rows: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT receipt_entry_id, amount
         FROM customer_account_credit_refund_allocations
         WHERE refund_entry_id = $1
         ORDER BY id",
    )
    .bind(refund_out.entry_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(r1, dec!(40.00)), (r3, dec!(50.00)), (r2, dec!(10.00))]
    );

    // Idempotent replay writes no duplicate attribution rows.
    let replay = organox::customer_accounts::service::refund_credit(
        &pool,
        ana,
        organox::customer_accounts::service::RefundCreditInput {
            payment_method_id: method_a,
            amount: dec!(100.00),
            note: "split refund",
            idempotency_key: Some("split-ref-key"),
            actor,
        },
    )
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry_id, refund_out.entry_id);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_credit_refund_allocations WHERE refund_entry_id = $1",
    )
    .bind(refund_out.entry_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3);

    // Remaining $20 on R2 can be allocated, nothing more.
    let sale = seed_completed_sale(&pool, ana, dec!(50.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let charge_id = repository::record_charge(&mut tx, ana, sale, dec!(50.00), actor)
        .await
        .unwrap();
    let applied = repository::apply_available_credit(&mut tx, ana, sale, dec!(50.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(applied, dec!(20.00));

    // UPDATE append-only trigger.
    let upd_err = sqlx::query(
        "UPDATE customer_account_credit_refund_allocations SET amount = 1.00 WHERE refund_entry_id = $1",
    )
    .bind(refund_out.entry_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(upd_err.to_string().contains("append-only"));

    // Cross-customer attribution rejected by BEFORE INSERT trigger.
    let cross_err = sqlx::query(
        "INSERT INTO customer_account_credit_refund_allocations (id, refund_entry_id, receipt_entry_id, amount)
         VALUES ($1, $2, $3, 1.00)",
    )
    .bind(Uuid::now_v7())
    .bind(refund_out.entry_id)
    .bind(beto_r)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(cross_err.to_string().contains("another customer"));

    // Wrong kinds rejected by BEFORE INSERT trigger.
    let bad_receipt_err = sqlx::query(
        "INSERT INTO customer_account_credit_refund_allocations (id, refund_entry_id, receipt_entry_id, amount)
         VALUES ($1, $2, $3, 1.00)",
    )
    .bind(Uuid::now_v7())
    .bind(refund_out.entry_id)
    .bind(charge_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        bad_receipt_err
            .to_string()
            .contains("only consume a receipt")
    );

    let bad_refund_err = sqlx::query(
        "INSERT INTO customer_account_credit_refund_allocations (id, refund_entry_id, receipt_entry_id, amount)
         VALUES ($1, $2, $3, 1.00)",
    )
    .bind(Uuid::now_v7())
    .bind(r1)
    .bind(r2)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(bad_refund_err.to_string().contains("only a credit_refund"));

    // Over-attributing an already-committed refund rejected at commit by fit_refund trigger.
    let mv_r4 = insert_mv(ana, wallet_b, method_b, "account_payment", dec!(15.00)).await;
    let mut tx = pool.begin().await.unwrap();
    let r4 = repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: mv_r4,
            amount: dec!(15.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO customer_account_credit_refund_allocations (id, refund_entry_id, receipt_entry_id, amount)
         VALUES ($1, $2, $3, 1.00)",
    )
    .bind(Uuid::now_v7())
    .bind(refund_out.entry_id)
    .bind(r4)
    .execute(&mut *tx)
    .await
    .unwrap();
    let over_err = tx.commit().await.unwrap_err();
    assert!(
        over_err
            .to_string()
            .contains("exceed the credit_refund amount"),
        "expected fit_refund trigger error, got: {over_err}"
    );
}

#[sqlx::test(migrations = false)]
async fn migration_0033_backfill_handles_clean_and_pre_fix_corrupted_ledgers(pool: PgPool) {
    let mut pre_0033: Vec<_> = std::fs::read_dir("migrations")
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".sql") && !name.ends_with(".down.sql") && name.as_str() < "0033_"
        })
        .collect();
    pre_0033.sort_by_key(|e| e.file_name());
    assert!(!pre_0033.is_empty(), "expected migrations 0001..0032");

    for entry in pre_0033 {
        let sql = std::fs::read_to_string(entry.path()).expect("read migration");
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap_or_else(|err| panic!("failed migration {:?}: {err}", entry.file_name()));
    }

    let actor = Uuid::now_v7();
    let wallet_a = Uuid::now_v7();
    let method_a = Uuid::now_v7();
    let wallet_b = Uuid::now_v7();
    let method_b = Uuid::now_v7();
    for (w, wn, m, mn) in [
        (wallet_a, "Backfill Cash", method_a, "Backfill Cash M"),
        (wallet_b, "Backfill Bank", method_b, "Backfill Bank M"),
    ] {
        sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
            .bind(w)
            .bind(wn)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
            .bind(m)
            .bind(mn)
            .bind(w)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Customer A: pre-0033 Issue #102 bug state + adjustment_out.
    // R1 ($100, Wallet A, t1), Refund ($100, Wallet A, t2), R2 ($50, Wallet B, t3),
    // Sale 1 ($50) allocated to R1 ($50), plus adjustment_out ($10).
    let cust_a = seed_customer(&pool, "Pre0033 Bug Customer").await;
    let sale_a = seed_completed_sale(&pool, cust_a, dec!(50.00)).await;
    let mv_ca_r1 = Uuid::now_v7();
    let mv_ca_ref = Uuid::now_v7();
    let mv_ca_r2 = Uuid::now_v7();
    for (id, w, m, kind, amt) in [
        (
            mv_ca_r1,
            wallet_a,
            method_a,
            "account_payment",
            dec!(100.00),
        ),
        (
            mv_ca_ref,
            wallet_a,
            method_a,
            "account_payment_refund",
            dec!(100.00),
        ),
        (mv_ca_r2, wallet_b, method_b, "account_payment", dec!(50.00)),
    ] {
        sqlx::query(
            "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
             VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)",
        )
        .bind(id)
        .bind(w)
        .bind(kind)
        .bind(amt)
        .bind(m)
        .bind(cust_a)
        .execute(&pool)
        .await
        .unwrap();
    }
    let ca_r1 = Uuid::now_v7();
    let ca_ref = Uuid::now_v7();
    let ca_r2 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by, created_at)
         VALUES ($1, $2, 'receipt', -100.00, $3, $4, now() - interval '3 hours'),
                ($5, $2, 'credit_refund', 100.00, $6, $4, now() - interval '2 hours'),
                ($7, $2, 'receipt', -50.00, $8, $4, now() - interval '1 hour')",
    )
    .bind(ca_r1)
    .bind(cust_a)
    .bind(mv_ca_r1)
    .bind(actor)
    .bind(ca_ref)
    .bind(mv_ca_ref)
    .bind(ca_r2)
    .bind(mv_ca_r2)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, created_by)
         VALUES ($1, $2, 'sale_charge', 50.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(cust_a)
    .bind(sale_a)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, note, created_by)
         VALUES ($1, $2, 'adjustment_out', -10.00, 'pre-fix adj', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(cust_a)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
         VALUES ($1, $2, $3, 50.00)",
    )
    .bind(Uuid::now_v7())
    .bind(ca_r1)
    .bind(sale_a)
    .execute(&pool)
    .await
    .unwrap();

    // Customer B: clean multi-receipt with same-wallet refund.
    // R1 ($40, Wallet A, t1), R2 ($60, Wallet B, t2), Refund ($60, Wallet B, t3).
    let cust_b = seed_customer(&pool, "Pre0033 Clean Customer").await;
    let mv_cb_r1 = Uuid::now_v7();
    let mv_cb_r2 = Uuid::now_v7();
    let mv_cb_ref = Uuid::now_v7();
    for (id, w, m, kind, amt) in [
        (mv_cb_r1, wallet_a, method_a, "account_payment", dec!(40.00)),
        (mv_cb_r2, wallet_b, method_b, "account_payment", dec!(60.00)),
        (
            mv_cb_ref,
            wallet_b,
            method_b,
            "account_payment_refund",
            dec!(60.00),
        ),
    ] {
        sqlx::query(
            "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
             VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)",
        )
        .bind(id)
        .bind(w)
        .bind(kind)
        .bind(amt)
        .bind(m)
        .bind(cust_b)
        .execute(&pool)
        .await
        .unwrap();
    }
    let cb_r1 = Uuid::now_v7();
    let cb_r2 = Uuid::now_v7();
    let cb_ref = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by, created_at)
         VALUES ($1, $2, 'receipt', -40.00, $3, $4, now() - interval '3 hours'),
                ($5, $2, 'receipt', -60.00, $6, $4, now() - interval '2 hours'),
                ($7, $2, 'credit_refund', 60.00, $8, $4, now() - interval '1 hour')",
    )
    .bind(cb_r1)
    .bind(cust_b)
    .bind(mv_cb_r1)
    .bind(actor)
    .bind(cb_r2)
    .bind(mv_cb_r2)
    .bind(cb_ref)
    .bind(mv_cb_ref)
    .execute(&pool)
    .await
    .unwrap();

    // Supplier S: pre-0033 Issue #102 bug state + purchase_return_credit.
    let supp_s = Uuid::now_v7();
    sqlx::query("INSERT INTO suppliers (id, name) VALUES ($1, 'Pre0033 Supplier')")
        .bind(supp_s)
        .execute(&pool)
        .await
        .unwrap();
    let purch_s = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, supplier_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'completed', $2, 'Pre0033 Supplier', 50.00, $3, $3, now())",
    )
    .bind(purch_s)
    .bind(supp_s)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    let ret_s = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 50.00, 'supplier_credit', 'pre-0033 credit', $3)",
    )
    .bind(ret_s)
    .bind(purch_s)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    let mv_sp1 = Uuid::now_v7();
    let mv_sref = Uuid::now_v7();
    for (id, kind) in [
        (mv_sp1, "supplier_account_payment"),
        (mv_sref, "supplier_account_refund"),
    ] {
        sqlx::query(
            "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, supplier_id)
             VALUES ($1, $2, $3::wallet_movement_kind, 100.00, $4, $5)",
        )
        .bind(id)
        .bind(wallet_a)
        .bind(kind)
        .bind(method_a)
        .bind(supp_s)
        .execute(&pool)
        .await
        .unwrap();
    }
    let sp1 = Uuid::now_v7();
    let sref = Uuid::now_v7();
    let src = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, wallet_movement_id, created_by, created_at)
         VALUES ($1, $2, 'payment', -100.00, $3, $4, now() - interval '3 hours'),
                ($5, $2, 'credit_refund', 100.00, $6, $4, now() - interval '2 hours')",
    )
    .bind(sp1)
    .bind(supp_s)
    .bind(mv_sp1)
    .bind(actor)
    .bind(sref)
    .bind(mv_sref)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, purchase_return_id, created_by, created_at)
         VALUES ($1, $2, 'purchase_return_credit', -50.00, $3, $4, now() - interval '1 hour')",
    )
    .bind(src)
    .bind(supp_s)
    .bind(ret_s)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, purchase_id, created_by)
         VALUES ($1, $2, 'purchase_charge', 50.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(supp_s)
    .bind(purch_s)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount)
         VALUES ($1, $2, $3, 50.00)",
    )
    .bind(Uuid::now_v7())
    .bind(sp1)
    .bind(purch_s)
    .execute(&pool)
    .await
    .unwrap();

    // Now run migration 0033 on top of the populated pre-0033 database!
    let sql_0033 = std::fs::read_to_string("migrations/0033_credit_refund_allocations.sql")
        .expect("read migration 0033");
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql_0033))
        .execute(&pool)
        .await
        .expect("migration 0033 backfill must succeed");

    // Verify Customer A backfill: $50 on R1 (remaining unallocated) + $50 on R2.
    let ca_attrs: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT receipt_entry_id, amount
         FROM customer_account_credit_refund_allocations
         WHERE refund_entry_id = $1
         ORDER BY receipt_entry_id",
    )
    .bind(ca_ref)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected_ca = vec![(ca_r1, dec!(50.00)), (ca_r2, dec!(50.00))];
    expected_ca.sort_by_key(|(id, _)| *id);
    assert_eq!(ca_attrs, expected_ca);

    // Verify Customer B backfill: $60 on R2 (same wallet), leaving R1 ($40) intact.
    let cb_attrs: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT receipt_entry_id, amount
         FROM customer_account_credit_refund_allocations
         WHERE refund_entry_id = $1",
    )
    .bind(cb_ref)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(cb_attrs, vec![(cb_r2, dec!(60.00))]);

    // Verify Supplier S backfill: $50 on Payment 1 + $50 on purchase_return_credit.
    let s_attrs: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT source_entry_id, amount
         FROM supplier_account_credit_refund_allocations
         WHERE refund_entry_id = $1
         ORDER BY source_entry_id",
    )
    .bind(sref)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected_s = vec![(sp1, dec!(50.00)), (src, dec!(50.00))];
    expected_s.sort_by_key(|(id, _)| *id);
    assert_eq!(s_attrs, expected_s);
}

#[sqlx::test]
async fn ensure_movement_backing_enforces_exact_kind_whitelist_for_receipts_and_refunds(
    pool: PgPool,
) {
    let actor = Uuid::now_v7();
    let ana = seed_customer(&pool, "Ana Whitelist").await;
    let sale = seed_completed_sale(&pool, ana, dec!(100.00)).await;
    let note = seed_credit_note(&pool, sale, dec!(25.00)).await;

    // Give Ana $100 of unapplied credit so affordability never masks the backing check.
    let initial_mv = seed_account_payment(&pool, ana, dec!(100.00)).await;
    let mut tx = pool.begin().await.expect("tx");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: initial_mv,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .expect("initial receipt");
    tx.commit().await.expect("commit");

    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, 'Whitelist Wallet')")
        .bind(wallet)
        .execute(&pool)
        .await
        .expect("wallet");
    sqlx::query(
        "INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, 'Whitelist Cash', $2)",
    )
    .bind(method)
    .bind(wallet)
    .execute(&pool)
    .await
    .expect("method");

    let supplier = Uuid::now_v7();
    sqlx::query("INSERT INTO suppliers (id, name) VALUES ($1, 'Whitelist Supplier')")
        .bind(supplier)
        .execute(&pool)
        .await
        .expect("supplier");
    let purchase = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, supplier_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'completed', $2, 'Whitelist Supplier', 100.00, $3, $3, now())",
    )
    .bind(purchase)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("purchase");
    let purchase_return = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 25.00, 'wallet_refund', 'whitelist test', $3)",
    )
    .bind(purchase_return)
    .bind(purchase)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("purchase return");

    let amount = dec!(25.00);
    let mut non_account_movements: Vec<(&str, Uuid)> = Vec::new();

    for kind in ["supplier_account_payment", "supplier_account_refund"] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, supplier_id)
             VALUES ($1, $2, $3::wallet_movement_kind, $4, $5, $6)",
        )
        .bind(id)
        .bind(wallet)
        .bind(kind)
        .bind(amount)
        .bind(method)
        .bind(supplier)
        .execute(&pool)
        .await
        .expect(kind);
        non_account_movements.push((kind, id));
    }

    let purchase_payment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, purchase_id)
         VALUES ($1, $2, 'purchase_payment', $3, $4, $5)",
    )
    .bind(purchase_payment_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(purchase)
    .execute(&pool)
    .await
    .expect("purchase_payment");
    non_account_movements.push(("purchase_payment", purchase_payment_id));

    let purchase_refund_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, purchase_id, purchase_return_id)
         VALUES ($1, $2, 'purchase_refund', $3, $4, $5, $6)",
    )
    .bind(purchase_refund_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(purchase)
    .bind(purchase_return)
    .execute(&pool)
    .await
    .expect("purchase_refund");
    non_account_movements.push(("purchase_refund", purchase_refund_id));

    let sale_payment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, sale_id)
         VALUES ($1, $2, 'sale_payment', $3, $4, $5)",
    )
    .bind(sale_payment_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(sale)
    .execute(&pool)
    .await
    .expect("sale_payment");
    non_account_movements.push(("sale_payment", sale_payment_id));

    let sale_refund_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, sale_id, credit_note_id)
         VALUES ($1, $2, 'sale_refund', $3, $4, $5, $6)",
    )
    .bind(sale_refund_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(sale)
    .bind(note)
    .execute(&pool)
    .await
    .expect("sale_refund");
    non_account_movements.push(("sale_refund", sale_refund_id));

    for kind in ["expense", "adjustment_in", "adjustment_out"] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO wallet_movements (id, wallet_id, kind, amount)
             VALUES ($1, $2, $3::wallet_movement_kind, $4)",
        )
        .bind(id)
        .bind(wallet)
        .bind(kind)
        .bind(amount)
        .execute(&pool)
        .await
        .expect(kind);
        non_account_movements.push((kind, id));
    }

    let account_payment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment', $3, $4, $5)",
    )
    .bind(account_payment_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(ana)
    .execute(&pool)
    .await
    .expect("account_payment");

    let account_payment_refund_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment_refund', $3, $4, $5)",
    )
    .bind(account_payment_refund_id)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .bind(ana)
    .execute(&pool)
    .await
    .expect("account_payment_refund");

    // Receipts accept only `account_payment`.
    for &(expected_kind, mv_id) in non_account_movements.iter().chain(std::iter::once(&(
        "account_payment_refund",
        account_payment_refund_id,
    ))) {
        let mut tx = pool.begin().await.expect("tx");
        let err = repository::record_receipt(
            &mut tx,
            repository::NewReceipt {
                customer_id: ana,
                wallet_movement_id: mv_id,
                amount,
                allocations: &[],
                idempotency_key: None,
                note: None,
                created_by: actor,
            },
        )
        .await
        .err();
        assert!(
            matches!(
                err,
                Some(repository::RepoError::MovementNotFromAnAccount { ref kind })
                    if kind == expected_kind
            ),
            "record_receipt must reject `{expected_kind}` with MovementNotFromAnAccount, got {err:?}",
        );
        tx.rollback().await.expect("rollback");
    }

    // Credit refunds accept only `account_payment_refund`.
    for &(expected_kind, mv_id) in non_account_movements
        .iter()
        .chain(std::iter::once(&("account_payment", account_payment_id)))
    {
        let mut tx = pool.begin().await.expect("tx");
        let err = repository::record_credit_refund(
            &mut tx,
            ana,
            mv_id,
            amount,
            Some("wrong kind"),
            actor,
        )
        .await;
        assert!(
            matches!(
                err,
                Err(repository::RepoError::MovementNotFromAnAccount { ref kind })
                    if kind == expected_kind
            ),
            "record_credit_refund must reject `{expected_kind}` with MovementNotFromAnAccount, got {err:?}",
        );
        tx.rollback().await.expect("rollback");
    }

    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
            .bind(ana)
            .fetch_one(&pool)
            .await
            .expect("entry count");
    assert_eq!(entries, 1, "rejected writes must not leave entries behind");
}

#[sqlx::test]
async fn a_credit_refund_backed_by_another_customers_movement_is_rejected(pool: PgPool) {
    let actor = Uuid::now_v7();
    let ana = seed_customer(&pool, "Ana Refund Owner").await;
    let beto = seed_customer(&pool, "Beto Refund Owner").await;

    let ana_payment = seed_account_payment(&pool, ana, dec!(100.00)).await;
    let mut tx = pool.begin().await.expect("tx");
    repository::record_receipt(
        &mut tx,
        repository::NewReceipt {
            customer_id: ana,
            wallet_movement_id: ana_payment,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .expect("receipt");
    tx.commit().await.expect("commit");

    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    let beto_refund_mv = Uuid::now_v7();
    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, 'Beto Refund Wallet')")
        .bind(wallet)
        .execute(&pool)
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, 'Beto Cash', $2)")
        .bind(method)
        .bind(wallet)
        .execute(&pool)
        .await
        .expect("method");
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id, customer_id)
         VALUES ($1, $2, 'account_payment_refund', 50.00, $3, $4)",
    )
    .bind(beto_refund_mv)
    .bind(wallet)
    .bind(method)
    .bind(beto)
    .execute(&pool)
    .await
    .expect("beto refund movement");

    let mut tx = pool.begin().await.expect("tx");
    let err = repository::record_credit_refund(
        &mut tx,
        ana,
        beto_refund_mv,
        dec!(50.00),
        Some("wrong owner"),
        actor,
    )
    .await;
    assert!(
        matches!(err, Err(repository::RepoError::MovementOfAnotherCustomer)),
        "expected MovementOfAnotherCustomer, got {err:?}",
    );
    tx.rollback().await.expect("rollback");

    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1 AND kind = 'credit_refund'",
    )
    .bind(ana)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 0);
}
