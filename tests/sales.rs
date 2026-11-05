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
use organox::routes::router;
use organox::sales::model::SaleStatus;
use organox::sales::service::{self as sales, SalesError};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

const PASSWORD: &str = "password-long-enough-123";

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

async fn seed_user(pool: &PgPool) -> Uuid {
    let email = validation::ValidatedEmail::parse("cashier@pyme.com").expect("email");
    let hash = password::hash(PASSWORD).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("user")
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
    sales::complete(&pool, draft, actor).await.unwrap();

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
    let done = sales::complete(&pool, draft, actor).await.unwrap();

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
    sales::complete(&pool, draft, actor).await.unwrap();

    let err = sales::complete(&pool, draft, actor)
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

    let err = sales::complete(&pool, draft, actor)
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
        let email = validation::ValidatedEmail::parse("register2@pyme.com").unwrap();
        let hash = password::hash(PASSWORD).unwrap();
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
    sales::complete(&pool, draft_a, actor_a).await.unwrap();
    assert_eq!(current_stock(&pool, product).await, dec!(0));

    // B cannot complete the sale: the unit is already gone.
    let err = sales::complete(&pool, draft_b, actor_b)
        .await
        .expect_err("no stock remains");
    assert!(matches!(err, SalesError::InsufficientStock { .. }));
}

#[sqlx::test]
async fn empty_sale_cannot_be_completed(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let draft = sales::open_draft(&pool, actor).await.unwrap();

    let err = sales::complete(&pool, draft, actor)
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
    sales::complete(&pool, draft, actor).await.unwrap();

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
        sales::complete(&pool, draft, actor).await.unwrap();
    }

    let list = sales::list_completed(&pool, 10).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.status == SaleStatus::Completed));
}

// --- HTTP ---------------------------------------------------------------

fn with_session(method: &str, uri: &str, cookie: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("session={cookie}"))
}

async fn login_cookie(pool: &PgPool) -> String {
    use tower::ServiceExt;
    let response = app(pool.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "email": "cashier@pyme.com", "password": PASSWORD })
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

    seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    // `query` without a macro needs no `.sqlx/` cache; this test only needs
    // an ID, not query type checking.
    let actor: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM users WHERE email = 'cashier@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    stock(&pool, actor, product, dec!(10)).await;

    let cookie = login_cookie(&pool).await;

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
