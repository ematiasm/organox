//! Sales integration tests.
//!
//! What these tests protect, in order of importance:
//!  1. Completing a sale deducts stock exactly once.
//!  2. A sale with insufficient stock leaves NOTHING (no movements, completed
//!     sale, or modified cart).
//!  3. An abandoned draft never touches the stock ledger.
//!  4. The server calculates the total.

use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self as catalog, CreateProductInput, RecordMovementInput};
use organox::customers::service::{self as customers, CreateCustomerInput, UpdateCustomerInput};
use organox::payments::model::WalletMovementKind;
use organox::payments::service::{self as payments, CreateMethodInput, CreateWalletInput};
use organox::routes::router;
use organox::sales::model::{SalePaymentInput, SaleStatus};
use organox::sales::repository::CompletedSale;
use organox::sales::service::{self as sales, SalesError};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

/// A password generated per test run, so no credential is written down.
fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

/// Creates the cashier and returns both its ID and the generated password, so
/// an HTTP test can log in with the same value.
async fn seed_user_with_password(pool: &PgPool) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse("cashier@pyme.com").expect("email");
    let hash = password::hash(&password).expect("hash");
    let id = repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("user");
    (id, password)
}

async fn seed_user(pool: &PgPool) -> Uuid {
    seed_user_with_password(pool).await.0
}

async fn seed_product(pool: &PgPool, name: &str, price: Decimal) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(price),
            min_stock: None,
        },
    )
    .await
    .expect("product")
}

async fn seed_service(pool: &PgPool, name: &str, price: Decimal) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Service,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(price),
            min_stock: None,
        },
    )
    .await
    .expect("service")
}

async fn stock(pool: &PgPool, actor: Uuid, product: Uuid, qty: Decimal) {
    catalog::record_movement(
        pool,
        RecordMovementInput {
            product_id: product,
            reason: MovementReason::Purchase,
            quantity: qty,
            unit_cost: None,
            note: None,
            actor_id: actor,
        },
    )
    .await
    .expect("stock entry");
}

async fn current_stock(pool: &PgPool, product: Uuid) -> Decimal {
    catalog::get_product(pool, product)
        .await
        .expect("product")
        .stock
        .expect("stock")
}

/// A payment method is required to charge. Seeded once per test database so a
/// test that charges twice does not collide on the unique name.
async fn cash(pool: &PgPool) -> Uuid {
    let existing = sqlx::query_scalar("SELECT id FROM payment_methods WHERE name = 'Cash'")
        .fetch_optional(pool)
        .await
        .unwrap();
    if let Some(id) = existing {
        return id;
    }

    let wallet = payments::create_wallet(
        pool,
        CreateWalletInput {
            name: "Caja".to_string(),
        },
    )
    .await
    .expect("wallet");

    payments::create_method(
        pool,
        CreateMethodInput {
            name: "Cash".to_string(),
            wallet_id: wallet,
        },
    )
    .await
    .expect("method")
}

/// Charge the sale in full with cash. Every completion needs a payment, so this
/// keeps the tests about the rule they name instead of about plumbing.
async fn complete(
    pool: &PgPool,
    sale: Uuid,
    actor: Uuid,
    amount: Decimal,
) -> Result<CompletedSale, SalesError> {
    let method = cash(pool).await;
    sales::complete(
        pool,
        sale,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount,
        }],
    )
    .await
}

// --- Draft --------------------------------------------------------------

/// Reloading the sales screen must not create new carts.
#[sqlx::test]
async fn open_draft_reuses_the_open_one(pool: PgPool) {
    let actor = seed_user(&pool).await;

    let first = sales::open_draft(&pool, actor).await.unwrap();
    let second = sales::open_draft(&pool, actor).await.unwrap();

    assert_eq!(first, second, "must return the same open draft");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sales WHERE status = 'draft'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// After completing a sale, the next `open_draft` creates a new draft.
#[sqlx::test]
async fn open_draft_creates_new_after_completion(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    complete(&pool, draft, actor, dec!(100)).await.unwrap();

    let next = sales::open_draft(&pool, actor).await.unwrap();
    assert_ne!(draft, next, "after completion, the next draft is new");
}

// --- Items --------------------------------------------------------------

#[sqlx::test]
async fn adding_the_same_product_sums_quantity_and_freezes_price(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();
    let sale = sales::add_item(&pool, draft, product, dec!(3))
        .await
        .unwrap();

    assert_eq!(sale.item_count(), 1, "the same product is a single row");
    assert_eq!(sale.items[0].quantity, dec!(5));
    // The price comes from the product and is frozen in the item.
    assert_eq!(sale.items[0].unit_price, dec!(1500.50));
    assert_eq!(sale.total(), dec!(7502.50));
}

/// Changing the product price does NOT change the open cart total: the item
/// froze the price when it was added.
#[sqlx::test]
async fn price_change_does_not_alter_an_open_draft(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    catalog::update_product(
        &pool,
        product,
        catalog::UpdateProductInput {
            sku: None,
            name: None,
            description: None,
            price: Some(dec!(999)),
            min_stock: None,
            is_active: None,
        },
    )
    .await
    .unwrap();

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.total(), dec!(200), "the price was frozen when added");
}

#[sqlx::test]
async fn service_cannot_be_sold_as_item(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let service = seed_service(&pool, "Delivery", dec!(8000)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let err = sales::add_item(&pool, draft, service, dec!(1))
        .await
        .expect_err("a service has no stock and cannot be sold as an item");

    assert!(matches!(err, SalesError::ServiceNotSellable));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sale_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn quantity_must_be_positive(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let err = sales::add_item(&pool, draft, product, dec!(0))
        .await
        .expect_err("quantity 0");
    assert!(matches!(err, SalesError::QuantityNotPositive));
}

#[sqlx::test]
async fn set_quantity_zero_removes_the_item(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    let sale = sales::set_item_quantity(&pool, draft, product, dec!(0))
        .await
        .unwrap();
    assert_eq!(sale.item_count(), 0);
    assert_eq!(sale.total(), Decimal::ZERO);
}

// --- Completion ---------------------------------------------------------

#[sqlx::test]
async fn completing_a_sale_discounts_stock_exactly_once(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(3))
        .await
        .unwrap();
    let done = complete(&pool, draft, actor, dec!(4501.50)).await.unwrap();

    assert_eq!(done.total, dec!(4501.50));
    assert_eq!(done.movement_ids.len(), 1);
    assert_eq!(current_stock(&pool, product).await, dec!(7));

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Completed);
    assert!(sale.completed_at.is_some());

    // Only one outgoing movement: completing the same draft twice is impossible
    // because it is no longer editable.
    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 1);
}

#[sqlx::test]
async fn completing_twice_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    complete(&pool, draft, actor, dec!(100)).await.unwrap();

    let err = sales::complete(&pool, draft, actor, &[])
        .await
        .expect_err("already completed");
    assert!(matches!(err, SalesError::NotEditable));

    // Stock was deducted only once.
    assert_eq!(current_stock(&pool, product).await, dec!(9));
}

/// The most important test: without stock, completion fails and **leaves nothing**.
#[sqlx::test]
async fn insufficient_stock_leaves_no_state_change(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(2)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(5))
        .await
        .unwrap();

    let err = complete(&pool, draft, actor, dec!(500))
        .await
        .expect_err("insufficient stock");

    match err {
        SalesError::InsufficientStock {
            product_name,
            available,
            requested,
        } => {
            assert_eq!(product_name, "Flour");
            assert_eq!(available, dec!(2));
            assert_eq!(requested, dec!(5));
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Nothing changed: status, stock, and sale movements are unchanged.
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft, "still a draft");
    assert_eq!(current_stock(&pool, product).await, dec!(2));

    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 0);
}

/// Register A sees the draft, not deducted stock. Stock falls only upon
/// completion, at which point B's draft becomes invalid.
#[sqlx::test]
async fn two_drafts_competing_for_the_last_units(pool: PgPool) {
    let actor_a = seed_user(&pool).await;
    let actor_b = {
        let password_b = test_password();
        let email = validation::ValidatedEmail::parse("register2@pyme.com").unwrap();
        let hash = password::hash(&password_b).unwrap();
        repository::create_user(
            &pool,
            NewUser {
                email: &email,
                password_hash: &hash,
                role: Role::Manager,
            },
        )
        .await
        .unwrap()
    };

    let product = seed_product(&pool, "Last unit", dec!(500)).await;
    stock(&pool, actor_a, product, dec!(1)).await;

    let draft_a = sales::open_draft(&pool, actor_a).await.unwrap();
    let draft_b = sales::open_draft(&pool, actor_b).await.unwrap();
    sales::add_item(&pool, draft_a, product, dec!(1))
        .await
        .unwrap();
    sales::add_item(&pool, draft_b, product, dec!(1))
        .await
        .unwrap();

    // A completes the sale: it wins.
    complete(&pool, draft_a, actor_a, dec!(500)).await.unwrap();
    assert_eq!(current_stock(&pool, product).await, dec!(0));

    // B cannot complete the sale: the unit is already gone.
    let err = complete(&pool, draft_b, actor_b, dec!(500))
        .await
        .expect_err("no stock remains");
    assert!(matches!(err, SalesError::InsufficientStock { .. }));
}

#[sqlx::test]
async fn empty_sale_cannot_be_completed(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let draft = sales::open_draft(&pool, actor).await.unwrap();

    let err = sales::complete(&pool, draft, actor, &[])
        .await
        .expect_err("empty cart");
    assert!(matches!(err, SalesError::EmptySale));
}

/// An abandoned draft never touches the stock ledger: stock is not deducted
/// until completion.
#[sqlx::test]
async fn abandoned_draft_never_touches_stock_ledger(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(4))
        .await
        .unwrap();
    sales::cancel(&pool, draft).await.unwrap();

    assert_eq!(current_stock(&pool, product).await, dec!(10));

    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 0);
}

#[sqlx::test]
async fn cancelling_a_completed_sale_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    complete(&pool, draft, actor, dec!(100)).await.unwrap();

    let err = sales::cancel(&pool, draft)
        .await
        .expect_err("already completed");
    assert!(matches!(err, SalesError::NotEditable));
}

#[sqlx::test]
async fn completed_sales_are_listed_newest_first(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    for _ in 0..2 {
        let draft = sales::open_draft(&pool, actor).await.unwrap();
        sales::add_item(&pool, draft, product, dec!(1))
            .await
            .unwrap();
        complete(&pool, draft, actor, dec!(100)).await.unwrap();
    }

    let list = sales::list_completed(&pool, 10).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.status == SaleStatus::Completed));
}

// --- Customer -----------------------------------------------------------

async fn seed_customer(pool: &PgPool, name: &str) -> Uuid {
    customers::create_customer(
        pool,
        CreateCustomerInput {
            name: name.to_string(),
            tax_id: None,
            email: None,
            phone: None,
        },
    )
    .await
    .expect("customer")
}

async fn rename_customer(pool: &PgPool, id: Uuid, name: &str) {
    customers::update_customer(
        pool,
        id,
        UpdateCustomerInput {
            name: Some(name.to_string()),
            tax_id: None,
            email: None,
            phone: None,
            is_active: None,
        },
    )
    .await
    .expect("rename");
}

async fn stored_customer_id(pool: &PgPool, sale: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT customer_id FROM sales WHERE id = $1")
        .bind(sale)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn stored_customer_name(pool: &PgPool, sale: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT customer_name FROM sales WHERE id = $1")
        .bind(sale)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Attaching writes the column; clearing returns it to NULL. Both only on a
/// draft.
#[sqlx::test]
async fn a_draft_accepts_and_then_clears_a_customer(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    assert_eq!(stored_customer_id(&pool, draft).await, Some(customer));
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.customer_id, Some(customer));
    assert_eq!(sale.customer_name.as_deref(), Some("Ana Pérez"));

    sales::set_customer(&pool, draft, None).await.unwrap();

    assert_eq!(stored_customer_id(&pool, draft).await, None);
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.customer_id, None);
    assert_eq!(sale.customer_name, None);
}

/// A completed or a cancelled sale is a fact: neither accepts a customer.
#[sqlx::test]
async fn a_closed_sale_refuses_a_customer_change(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    complete(&pool, draft, actor, dec!(100)).await.unwrap();

    let err = sales::set_customer(&pool, draft, Some(customer))
        .await
        .expect_err("a completed sale cannot take a customer");
    assert!(matches!(err, SalesError::NotEditable));
    assert_eq!(stored_customer_id(&pool, draft).await, None);

    let cancelled = sales::open_draft(&pool, actor).await.unwrap();
    sales::cancel(&pool, cancelled).await.unwrap();
    let err = sales::set_customer(&pool, cancelled, Some(customer))
        .await
        .expect_err("a cancelled draft cannot take a customer");
    assert!(matches!(err, SalesError::NotEditable));
}

/// Unknown id: the draft is left untouched.
#[sqlx::test]
async fn an_unknown_customer_cannot_be_attached(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let draft = sales::open_draft(&pool, actor).await.unwrap();

    let err = sales::set_customer(&pool, draft, Some(Uuid::now_v7()))
        .await
        .expect_err("no such customer");
    assert!(matches!(err, SalesError::NotFound));
    assert_eq!(stored_customer_id(&pool, draft).await, None);
}

/// The active check is the customers module's rule, not a copy in sales SQL.
#[sqlx::test]
async fn a_deactivated_customer_cannot_be_attached(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    customers::deactivate_customer(&pool, customer)
        .await
        .unwrap();

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let err = sales::set_customer(&pool, draft, Some(customer))
        .await
        .expect_err("a deactivated customer cannot be attached");
    assert!(matches!(err, SalesError::CustomerInactive));
    assert_eq!(stored_customer_id(&pool, draft).await, None);
}

/// While the sale is a draft the name follows the registry (the sale is still
/// editable, so the name is too); after completion the frozen snapshot wins,
/// so a rename never rewrites what the charged sale says.
#[sqlx::test]
async fn the_customer_name_freezes_at_completion(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    // Draft: the name comes from the joined customer and tracks a rename.
    rename_customer(&pool, customer, "Ana Gómez").await;
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.customer_name.as_deref(), Some("Ana Gómez"));
    assert_eq!(
        stored_customer_name(&pool, draft).await,
        None,
        "a draft stores no snapshot yet"
    );

    complete(&pool, draft, actor, dec!(100)).await.unwrap();
    assert_eq!(
        stored_customer_name(&pool, draft).await.as_deref(),
        Some("Ana Gómez")
    );

    rename_customer(&pool, customer, "Ana Smith").await;
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(
        sale.customer_name.as_deref(),
        Some("Ana Gómez"),
        "the snapshot wins after completion"
    );
}

/// A customer deactivated between the attach and the checkout does NOT block
/// the sale: the goods already left, and the snapshot is still taken.
#[sqlx::test]
async fn a_customer_deactivated_mid_draft_does_not_block_checkout(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();
    customers::deactivate_customer(&pool, customer)
        .await
        .unwrap();

    complete(&pool, draft, actor, dec!(100)).await.unwrap();
    assert_eq!(
        stored_customer_name(&pool, draft).await.as_deref(),
        Some("Ana Pérez")
    );
}

// --- Payment ------------------------------------------------------------

async fn wallet_for(pool: &PgPool, method: Uuid) -> Uuid {
    payments::list_methods(pool, true)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == method)
        .expect("method")
        .wallet_id
}

async fn wallet_balance(pool: &PgPool, wallet: Uuid) -> Decimal {
    payments::get_wallet(pool, wallet).await.unwrap().balance
}

async fn payment_movement_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM wallet_movements")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Charging a sale must move the money, not only the stock: the same operation,
/// the same transaction.
#[sqlx::test]
async fn charging_a_sale_credits_the_method_wallet(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let done = sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(3001.00),
        }],
    )
    .await
    .unwrap();

    assert_eq!(done.payment_movement_ids.len(), 1);
    assert_eq!(wallet_balance(&pool, wallet).await, dec!(3001.00));

    let movements = payments::list_movements(&pool, wallet, 10).await.unwrap();
    assert_eq!(movements.len(), 1);
    assert_eq!(movements[0].kind, WalletMovementKind::SalePayment);
    assert_eq!(movements[0].sale_id, Some(draft));
    assert_eq!(movements[0].payment_method_id, Some(method));
    assert_eq!(movements[0].amount, dec!(3001.00));
}

/// Half cash, half card: one sale, two wallets, two ledger lines.
#[sqlx::test]
async fn a_split_payment_credits_every_wallet(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let cash_method = cash(&pool).await;
    let cash_wallet = wallet_for(&pool, cash_method).await;
    let card_wallet = payments::create_wallet(
        &pool,
        CreateWalletInput {
            name: "Banco".to_string(),
        },
    )
    .await
    .unwrap();
    let card_method = payments::create_method(
        &pool,
        CreateMethodInput {
            name: "Card".to_string(),
            wallet_id: card_wallet,
        },
    )
    .await
    .unwrap();

    sales::complete(
        &pool,
        draft,
        actor,
        &[
            SalePaymentInput {
                payment_method_id: cash_method,
                amount: dec!(60),
            },
            SalePaymentInput {
                payment_method_id: card_method,
                amount: dec!(40),
            },
        ],
    )
    .await
    .unwrap();

    assert_eq!(wallet_balance(&pool, cash_wallet).await, dec!(60));
    assert_eq!(wallet_balance(&pool, card_wallet).await, dec!(40));
    assert_eq!(payment_movement_count(&pool).await, 2);
}

/// A payment that does not match the total is not "almost right": the sale stays
/// a draft and nothing enters either ledger.
#[sqlx::test]
async fn a_payment_that_does_not_add_up_leaves_no_trace(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let err = sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(150),
        }],
    )
    .await
    .expect_err("150 is not the 200 total");

    match err {
        SalesError::PaymentMismatch {
            total,
            paid,
            on_account,
        } => {
            assert_eq!(total, dec!(200));
            assert_eq!(paid, dec!(150));
            assert_eq!(on_account, Decimal::ZERO);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(wallet_balance(&pool, wallet).await, Decimal::ZERO);

    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 0);
}

/// An empty payment list cannot cover a positive total on its own: nothing is
/// collected and nothing is owed, so there is nothing to complete the sale
/// with. (The account can cover it — that is the on-account section's rule.)
#[sqlx::test]
async fn an_empty_payment_list_cannot_cover_a_positive_total_on_its_own(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let err = sales::complete(&pool, draft, actor, &[])
        .await
        .expect_err("a sale cannot close without saying how it was paid");
    assert!(matches!(
        err,
        SalesError::PaymentMismatch { total, paid, on_account }
            if total == dec!(100) && paid == Decimal::ZERO && on_account == Decimal::ZERO
    ));
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(payment_movement_count(&pool).await, 0);
}

/// A zero-total sale (a sample, a warranty replacement) has no money to route,
/// so it needs no payment. Before this feature it completed with no payments at
/// all, and making it impossible would be a regression dressed as a rule.
#[sqlx::test]
async fn a_zero_total_sale_needs_no_payment(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Sample", dec!(0)).await;
    stock(&pool, actor, product, dec!(5)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let done = sales::complete(&pool, draft, actor, &[])
        .await
        .expect("a 0.00 total has nothing to collect");

    assert_eq!(done.total, Decimal::ZERO);
    assert!(done.payment_movement_ids.is_empty());
    assert_eq!(current_stock(&pool, product).await, dec!(4));
    assert_eq!(payment_movement_count(&pool).await, 0);
}

/// The wallet on a movement is the one that received the money, not the one the
/// method points at today. Without this test, re-pointing a method could move
/// yesterday's money and nobody would notice.
#[sqlx::test]
async fn repointing_a_method_does_not_move_a_recorded_payment(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let method = cash(&pool).await;
    let cash_wallet = wallet_for(&pool, method).await;
    let bank = payments::create_wallet(
        &pool,
        CreateWalletInput {
            name: "Banco".to_string(),
        },
    )
    .await
    .unwrap();

    sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(100),
        }],
    )
    .await
    .unwrap();

    payments::update_method(
        &pool,
        method,
        organox::payments::service::UpdateMethodInput {
            name: None,
            wallet_id: Some(bank),
            is_active: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        wallet_balance(&pool, cash_wallet).await,
        dec!(100),
        "the money stays in the wallet that received it"
    );
    assert_eq!(wallet_balance(&pool, bank).await, Decimal::ZERO);
}

/// Three decimals are rejected instead of silently rounded: charging 100.005 as
/// 100.00 changes what the client asked for.
#[sqlx::test]
async fn a_payment_with_three_decimals_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let method = cash(&pool).await;
    let err = sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(100.005),
        }],
    )
    .await
    .expect_err("three decimals cannot be charged");

    assert!(matches!(err, SalesError::PaymentAmountPrecision));
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(payment_movement_count(&pool).await, 0);
}

#[sqlx::test]
async fn a_deactivated_method_cannot_charge(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let method = cash(&pool).await;
    payments::update_method(
        &pool,
        method,
        organox::payments::service::UpdateMethodInput {
            name: None,
            wallet_id: None,
            is_active: Some(false),
        },
    )
    .await
    .unwrap();

    let err = sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(100),
        }],
    )
    .await
    .expect_err("the method is inactive");
    assert!(matches!(err, SalesError::PaymentMethodUnavailable));
    assert_eq!(current_stock(&pool, product).await, dec!(10));
}

#[sqlx::test]
async fn an_unknown_method_cannot_charge(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let err = sales::complete(
        &pool,
        draft,
        actor,
        &[SalePaymentInput {
            payment_method_id: Uuid::now_v7(),
            amount: dec!(100),
        }],
    )
    .await
    .expect_err("unknown method");
    assert!(matches!(err, SalesError::PaymentMethodUnavailable));
}

// --- HTTP ---------------------------------------------------------------

fn with_session(method: &str, uri: &str, cookie: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("session={cookie}"))
}

async fn login_cookie(pool: &PgPool, password: &str) -> String {
    use tower::ServiceExt;
    let response = app(pool.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "email": "cashier@pyme.com", "password": password })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

#[sqlx::test]
async fn http_sale_flow_returns_server_computed_total(pool: PgPool) {
    use tower::ServiceExt;

    let (_, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    // `query` without a macro needs no `.sqlx/` cache; this test only needs
    // an ID, not query type checking.
    let actor: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM users WHERE email = 'cashier@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    stock(&pool, actor, product, dec!(10)).await;

    let cookie = login_cookie(&pool, &password).await;

    let draft = app(pool.clone())
        .oneshot(
            with_session("POST", "/api/v1/sales/draft", &cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        draft.status() == axum::http::StatusCode::CREATED
            || draft.status() == axum::http::StatusCode::OK
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(draft.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    let sale_id = body["id"].as_str().unwrap().to_string();

    let added = app(pool.clone())
        .oneshot(
            with_session("POST", &format!("/api/v1/sales/{sale_id}/items"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "product_id": product, "quantity": "2" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(added.status(), axum::http::StatusCode::OK);

    let sale: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(added.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();

    // Decimals as strings, with the total calculated by the server.
    assert_eq!(sale["total"], "3001.00");
    assert_eq!(sale["item_count"], 1);
    assert_eq!(sale["items"][0]["subtotal"], "3001.00");
}

/// The route and the response shape: 204 on attach and on clear, the two
/// fields present in the sale JSON, and `null` for both on a sale without a
/// customer.
#[sqlx::test]
async fn http_customer_route_attaches_clears_and_reports_the_fields(pool: PgPool) {
    use tower::ServiceExt;

    let (_, password) = seed_user_with_password(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    let cookie = login_cookie(&pool, &password).await;

    let draft = app(pool.clone())
        .oneshot(
            with_session("POST", "/api/v1/sales/draft", &cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(draft.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    let sale_id = body["id"].as_str().unwrap().to_string();

    // A sale without a customer reports null for both fields.
    let sale: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(
            app(pool.clone())
                .oneshot(
                    with_session("GET", &format!("/api/v1/sales/{sale_id}"), &cookie)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body(),
            65536,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sale["customer_id"], serde_json::Value::Null);
    assert_eq!(sale["customer_name"], serde_json::Value::Null);

    let put = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{sale_id}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "customer_id": customer }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), axum::http::StatusCode::NO_CONTENT);

    let sale: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(
            app(pool.clone())
                .oneshot(
                    with_session("GET", &format!("/api/v1/sales/{sale_id}"), &cookie)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body(),
            65536,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sale["customer_id"], serde_json::json!(customer));
    assert_eq!(sale["customer_name"], "Ana Pérez");

    // Clearing is an explicit null, not an omitted field.
    let put = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{sale_id}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "customer_id": null }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), axum::http::StatusCode::NO_CONTENT);

    let sale: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(
            app(pool.clone())
                .oneshot(
                    with_session("GET", &format!("/api/v1/sales/{sale_id}"), &cookie)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body(),
            65536,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sale["customer_id"], serde_json::Value::Null);
    assert_eq!(sale["customer_name"], serde_json::Value::Null);
}

/// The errors are RFC 7807 problem+json with the agreed codes: 409 for a
/// closed sale or a deactivated customer, 404 for an unknown one.
#[sqlx::test]
async fn http_customer_route_errors_are_problem_json(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let inactive = seed_customer(&pool, "Viejo").await;
    customers::deactivate_customer(&pool, inactive)
        .await
        .unwrap();

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    complete(&pool, draft, actor, dec!(100)).await.unwrap();

    let cookie = login_cookie(&pool, &password).await;

    // A closed sale: 409.
    let closed = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{draft}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "customer_id": null }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(closed.status(), axum::http::StatusCode::CONFLICT);
    let problem: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(closed.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        problem["detail"],
        "the sale is already closed and cannot be modified"
    );

    // Unknown customer: 404.
    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let unknown = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{draft}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "customer_id": Uuid::now_v7() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), axum::http::StatusCode::NOT_FOUND);

    // Deactivated customer: 409 with a readable detail.
    let inactive_put = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{draft}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "customer_id": inactive }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inactive_put.status(), axum::http::StatusCode::CONFLICT);
    let problem: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(inactive_put.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        problem["detail"],
        "the customer is deactivated and cannot be attached to a sale"
    );
}

/// `customer_id` is required in the body: an omitted field is a truncated
/// payload, not a request to clear the customer. Reading it as "clear" would
/// detach a customer silently, and the cashier would find out on the ticket.
#[sqlx::test]
async fn an_omitted_customer_id_is_rejected_and_keeps_the_customer(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    let cookie = login_cookie(&pool, &password).await;
    let response = app(pool.clone())
        .oneshot(
            with_session("PUT", &format!("/api/v1/sales/{draft}/customer"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "an omitted customer_id is not a request to clear"
    );
    let problem: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        problem["detail"],
        "customer_id is required: send null to clear the sale's customer"
    );

    // The rejected request left the stored row untouched.
    assert_eq!(stored_customer_id(&pool, draft).await, Some(customer));
}

// --- On account --------------------------------------------------------

/// The customer's account balance, read through the ledger's own derived
/// view: the tests assert what the API would report, not a re-implementation
/// of the sum.
async fn account_balance(pool: &PgPool, customer: Uuid) -> Decimal {
    organox::customer_accounts::repository::account_position(pool, customer)
        .await
        .expect("account position")
        .balance
}

/// How many `sale_charge` entries one sale produced. The ledger's unique
/// partial index keeps this at one; the tests prove the completion wrote it.
async fn sale_charge_count(pool: &PgPool, sale: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM customer_account_entries
         WHERE sale_id = $1 AND kind = 'sale_charge'",
    )
    .bind(sale)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn ledger_entry_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM customer_account_entries")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn stock_outflow_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// POST the completion body the clients send. The on-account rules go
/// through HTTP on purpose: the `on_account` field, its default and the 422
/// mappings are part of what this change pins, not just the rule behind them.
async fn http_complete(
    pool: &PgPool,
    cookie: &str,
    sale: Uuid,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    app(pool.clone())
        .oneshot(
            with_session("POST", &format!("/api/v1/sales/{sale}/complete"), cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// The `detail` of a rejected completion, as the client reads it.
async fn problem_detail(response: axum::http::Response<axum::body::Body>) -> String {
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    body["detail"].as_str().unwrap().to_string()
}

/// The common setup of the on-account tests: a cashier with a session, a
/// product at `price`, stock for ten units, and a draft holding one unit.
async fn draft_at_price(pool: &PgPool, price: Decimal) -> (Uuid, Uuid, Uuid, String) {
    let (actor, password) = seed_user_with_password(pool).await;
    let product = seed_product(pool, "Flour", price).await;
    stock(pool, actor, product, dec!(10)).await;
    let draft = sales::open_draft(pool, actor).await.unwrap();
    sales::add_item(pool, draft, product, dec!(1))
        .await
        .unwrap();
    let cookie = login_cookie(pool, &password).await;
    (actor, draft, product, cookie)
}

/// Part in a wallet, part on account: one wallet movement for the 40, one
/// ledger charge for the 60, one stock deduction — all from one completion.
#[sqlx::test]
async fn a_sale_can_be_completed_with_part_on_account(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();
    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({
            "payments": [ { "payment_method_id": method, "amount": "40.00" } ],
            "on_account": "60.00"
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["sale_id"], serde_json::json!(draft));
    assert_eq!(body["total"], "100.00");

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Completed);

    // The wallet took 40, the account took 60, the stock left once.
    assert_eq!(wallet_balance(&pool, wallet).await, dec!(40.00));
    assert_eq!(account_balance(&pool, customer).await, dec!(60.00));
    assert_eq!(sale_charge_count(&pool, draft).await, 1);
    assert_eq!(current_stock(&pool, product).await, dec!(9));
    assert_eq!(stock_outflow_count(&pool).await, 1);
}

/// The whole total on account: no wallet is touched at all, because no money
/// entered one; the account is the only record that the sale is owed.
#[sqlx::test]
async fn a_sale_can_be_completed_entirely_on_account(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "100.00" }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Completed);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, dec!(100.00));
    assert_eq!(sale_charge_count(&pool, draft).await, 1);
    assert_eq!(current_stock(&pool, product).await, dec!(9));
}

/// Debt needs a debtor: an on-account part on an anonymous draft is rejected
/// with nothing written.
#[sqlx::test]
async fn a_sale_on_account_needs_a_customer(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "100.00" }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "an on-account amount needs a customer on the sale"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
}

/// Credit is extended only to customers the registry still vouches for: a
/// customer deactivated before the checkout blocks the on-account part — and
/// only that part, the cash-only case is pinned by
/// `a_customer_deactivated_mid_draft_does_not_block_checkout`.
#[sqlx::test]
async fn a_sale_on_account_needs_an_active_customer(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();
    customers::deactivate_customer(&pool, customer)
        .await
        .unwrap();

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "100.00" }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "an on-account amount needs an active customer"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, Decimal::ZERO);
}

/// 40 in a wallet and 30 on account do not cover a total of 100: the
/// invariant is exact, and its rejection leaves nothing behind.
#[sqlx::test]
async fn on_account_and_payments_must_add_up_to_the_total(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();
    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({
            "payments": [ { "payment_method_id": method, "amount": "40.00" } ],
            "on_account": "30.00"
        }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the payments and the on-account amount add up to 70.00, but the sale total is 100.00"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(wallet_balance(&pool, wallet).await, Decimal::ZERO);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
}

/// More on account than the sale costs is a mismatch, not credit: the
/// invariant has no room for a part larger than the whole.
#[sqlx::test]
async fn on_account_cannot_exceed_the_total(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "120.00" }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the payments and the on-account amount add up to 120.00, but the sale total is 100.00"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, Decimal::ZERO);
}

/// Three decimals are rejected instead of silently rounded — the same rule a
/// payment with three decimals follows. Charging 10.005 as 10.00 would change
/// what the client asked for.
#[sqlx::test]
async fn an_on_account_with_three_decimals_is_rejected(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "10.005" }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the on-account amount has more than 2 decimal places"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, Decimal::ZERO);
}

/// A zero total keeps completing with no payments and no on-account part;
/// a positive on-account amount can never satisfy the invariant against a
/// total of zero.
#[sqlx::test]
async fn a_zero_total_sale_needs_no_on_account(pool: PgPool) {
    let (actor, draft, product, cookie) = draft_at_price(&pool, Decimal::ZERO).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "0" }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(current_stock(&pool, product).await, dec!(9));
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);

    // The same cashier's next draft: owing anything against a zero total is a
    // mismatch by construction.
    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [], "on_account": "1.00" }),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the payments and the on-account amount add up to 1.00, but the sale total is 0.00"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(9));
    // Only the first, accepted sale deducted stock.
    assert_eq!(stock_outflow_count(&pool).await, 1);
    assert_eq!(ledger_entry_count(&pool).await, 0);
}

/// A negative on-account amount is the only way 105 collected at the till can
/// balance a total of 100. It would leave the sale over-collected with nothing
/// in the ledger explaining the surplus, so the guard has to refuse it before
/// any write.
#[sqlx::test]
async fn a_negative_on_account_is_rejected(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();
    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({
            "payments": [ { "payment_method_id": method, "amount": "105.00" } ],
            "on_account": "-5.00"
        }),
    )
    .await;

    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the on-account amount cannot be negative"
    );

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(wallet_balance(&pool, wallet).await, Decimal::ZERO);
    assert_eq!(payment_movement_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, Decimal::ZERO);
}

/// The on-account charge waits for the customer's advisory lock, and that wait
/// is bounded. A collection in flight must not turn a checkout into a request
/// that hangs forever: the register gets a retryable 409 instead.
#[sqlx::test]
async fn a_busy_customer_account_answers_409(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let customer = seed_customer(&pool, "Ana Pérez").await;
    sales::set_customer(&pool, draft, Some(customer))
        .await
        .unwrap();

    // Another connection holds the account the way a collection in flight does.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(customer.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        http_complete(
            &pool,
            &cookie,
            draft,
            serde_json::json!({ "payments": [], "on_account": "100.00" }),
        ),
    )
    .await
    .expect("the completion never answered: the lock wait is not bounded");

    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        problem_detail(response).await,
        "the customer account is busy, retry"
    );

    holder.rollback().await.unwrap();

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(stock_outflow_count(&pool).await, 0);
    assert_eq!(ledger_entry_count(&pool).await, 0);
    assert_eq!(account_balance(&pool, customer).await, Decimal::ZERO);
}

/// The mismatch detail is part of the contract. A completion that never used
/// the account has to keep the sentence it always answered with: a change that
/// claims `on_account = 0` behaves as before must not quietly rewrite it.
#[sqlx::test]
async fn a_cash_only_mismatch_keeps_its_original_message(pool: PgPool) {
    let (_actor, draft, product, cookie) = draft_at_price(&pool, dec!(100)).await;
    let method = cash(&pool).await;
    let wallet = wallet_for(&pool, method).await;

    let response = http_complete(
        &pool,
        &cookie,
        draft,
        serde_json::json!({ "payments": [ { "payment_method_id": method, "amount": "90.00" } ] }),
    )
    .await;

    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        problem_detail(response).await,
        "the payments do not add up to the sale total"
    );

    assert_eq!(
        sales::get(&pool, draft).await.unwrap().status,
        SaleStatus::Draft
    );
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(wallet_balance(&pool, wallet).await, Decimal::ZERO);
    assert_eq!(payment_movement_count(&pool).await, 0);
}
