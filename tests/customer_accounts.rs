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

/// A wallet movement a receipt can point at. The ledger only needs the money to
/// exist in a wallet; which kind of movement carried it is not its business.
async fn seed_movement(pool: &PgPool, amount: Decimal) -> Uuid {
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
        INSERT INTO wallet_movements (id, wallet_id, kind, amount, payment_method_id)
        VALUES ($1, $2, 'adjustment_in', $3, $4)
        "#,
    )
    .bind(movement)
    .bind(wallet)
    .bind(amount)
    .bind(method)
    .execute(pool)
    .await
    .expect("movement");

    movement
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
    let movement = seed_movement(&pool, dec!(150.00)).await;

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
    let movement = seed_movement(&pool, dec!(150.00)).await;

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
    let movement = seed_movement(&pool, dec!(40.00)).await;

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
    let movement = seed_movement(&pool, dec!(50.00)).await;

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
    let movement = seed_movement(&pool, dec!(50.00)).await;

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
    let movement = seed_movement(&pool, dec!(100.00)).await;

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
    let movement = seed_movement(&pool, dec!(50.00)).await;

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
    let movement = seed_movement(&pool, dec!(100.00)).await;

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
    let movement = seed_movement(&pool, dec!(100.00)).await;

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
    let movement = seed_movement(&pool, dec!(60.00)).await;

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
    let movement = seed_movement(&pool, dec!(150.00)).await;

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
    let first_movement = seed_movement(&pool, dec!(60.00)).await;
    let second_movement = seed_movement(&pool, dec!(60.00)).await;

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

    let movement = seed_movement(&pool, dec!(100.00)).await;
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

    let movement = seed_movement(&pool, dec!(100.00)).await;
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

    let movement_a = seed_movement(&pool, dec!(60.00)).await;
    let movement_b = seed_movement(&pool, dec!(60.00)).await;

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
    let movement = seed_movement(&pool, dec!(100.00)).await;

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
    let collected = seed_movement(&pool, dec!(150.00)).await;
    let refunded = seed_movement(&pool, dec!(50.00)).await;

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
    let movement = seed_movement(&pool, dec!(10.00)).await;

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
    let collected = seed_movement(&pool, dec!(150.00)).await;
    let refunded = seed_movement(&pool, dec!(30.00)).await;

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
    let movement = seed_movement(&pool, dec!(150.00)).await;

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
