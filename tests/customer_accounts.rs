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
