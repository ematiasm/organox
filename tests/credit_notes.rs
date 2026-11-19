//! Credit-note integration tests.
//!
//! What these tests protect, in order of importance:
//!  1. A return gives back exactly what was returned: stock, money and a
//!     document that explains it.
//!  2. No sequence of notes can return more than was sold.
//!  3. A rejected note leaves NOTHING: no document, no stock movement, no money
//!     movement.
//!  4. The refund leaves the wallets that received the payment.

use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self as catalog, CreateProductInput, RecordMovementInput};
use organox::credit_notes::model::ReturnLine;
use organox::credit_notes::service::{self as credit_notes, CreditNotesError};
use organox::payments::service::{self as payments, CreateMethodInput, CreateWalletInput};
use organox::routes::router;
use organox::sales::model::SalePaymentInput;
use organox::sales::service as sales;
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

async fn balance(pool: &PgPool, wallet: Uuid) -> Decimal {
    payments::get_wallet(pool, wallet)
        .await
        .expect("wallet")
        .balance
}

async fn wallet_of(pool: &PgPool, method: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT wallet_id FROM payment_methods WHERE id = $1")
        .bind(method)
        .fetch_one(pool)
        .await
        .expect("method wallet")
}

/// The cash wallet and method, created once per test database so a test that
/// charges twice does not collide on the unique name.
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

/// A charged sale of `quantity` units, paid in cash.
async fn charged_sale(
    pool: &PgPool,
    actor: Uuid,
    product: Uuid,
    quantity: Decimal,
) -> (Uuid, Uuid, Decimal) {
    let sale = sales::open_draft(pool, actor).await.expect("draft");
    sales::add_item(pool, sale, product, quantity)
        .await
        .expect("item");
    let sale = sales::get(pool, sale).await.expect("sale");
    let total = sale.total();
    let method = cash(pool).await;

    sales::complete(
        pool,
        sale.id,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: total,
        }],
    )
    .await
    .expect("charge");

    (sale.id, method, total)
}

async fn note_count(pool: &PgPool, sale: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM credit_notes WHERE sale_id = $1")
        .bind(sale)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn return_movement_count(pool: &PgPool, product: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM stock_movements WHERE product_id = $1 AND reason = 'return'",
    )
    .bind(product)
    .fetch_one(pool)
    .await
    .expect("count")
}

fn line(product: Uuid, quantity: Decimal) -> ReturnLine {
    ReturnLine {
        product_id: product,
        quantity,
    }
}

// --- A return gives back what came back ---------------------------------

#[sqlx::test]
async fn a_full_return_gives_back_the_stock_and_the_money(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, method, total) = charged_sale(&pool, actor, product, dec!(3)).await;
    let wallet = wallet_of(&pool, method).await;

    assert_eq!(total, dec!(4501.50));
    assert_eq!(current_stock(&pool, product).await, dec!(7));
    assert_eq!(balance(&pool, wallet).await, dec!(4501.50));

    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "Customer changed their mind",
        &[line(product, dec!(3))],
    )
    .await
    .expect("credit note");

    assert_eq!(note.note.total, dec!(4501.50));
    assert_eq!(note.note.sale_id, sale);
    assert_eq!(note.items.len(), 1);
    assert_eq!(note.items[0].quantity, dec!(3));
    // The price is the one that was charged, not today's product price.
    assert_eq!(note.items[0].unit_price, dec!(1500.50));
    assert_eq!(note.items[0].subtotal(), dec!(4501.50));

    assert_eq!(note.refunds.len(), 1);
    assert_eq!(note.refunds[0].wallet_id, wallet);
    assert_eq!(note.refunds[0].wallet_name, "Caja");
    assert_eq!(note.refunds[0].payment_method_name, "Cash");
    assert_eq!(note.refunds[0].amount, dec!(4501.50));

    // The goods are back and the money is gone, both as ledger entries.
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(balance(&pool, wallet).await, Decimal::ZERO);
    assert_eq!(note_count(&pool, sale).await, 1);
}

#[sqlx::test]
async fn a_partial_return_moves_only_what_came_back(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(3)).await;
    let wallet = wallet_of(&pool, method).await;

    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "One was damaged",
        &[line(product, dec!(1))],
    )
    .await
    .expect("credit note");

    assert_eq!(note.note.total, dec!(1500.50));
    assert_eq!(current_stock(&pool, product).await, dec!(8));
    assert_eq!(balance(&pool, wallet).await, dec!(3001.00));
}

#[sqlx::test]
async fn a_sale_can_be_returned_in_two_notes_until_nothing_is_left(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(3)).await;
    let wallet = wallet_of(&pool, method).await;

    credit_notes::create(&pool, sale, actor, "First visit", &[line(product, dec!(1))])
        .await
        .expect("first note");
    credit_notes::create(
        &pool,
        sale,
        actor,
        "Second visit",
        &[line(product, dec!(2))],
    )
    .await
    .expect("second note");

    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(balance(&pool, wallet).await, Decimal::ZERO);
    assert_eq!(note_count(&pool, sale).await, 2);

    // Nothing is left: the third attempt is a conflict that names the zero.
    let err = credit_notes::create(&pool, sale, actor, "Third visit", &[line(product, dec!(1))])
        .await
        .expect_err("nothing left to return");
    assert!(
        matches!(
            err,
            CreditNotesError::ExceedsReturnable { available, .. } if available == Decimal::ZERO
        ),
        "unexpected error: {err}"
    );

    assert_eq!(note_count(&pool, sale).await, 2);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
}

// --- Rejections leave nothing -------------------------------------------

#[sqlx::test]
async fn returning_more_than_was_sold_writes_nothing(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(3)).await;
    let wallet = wallet_of(&pool, method).await;

    let err = credit_notes::create(&pool, sale, actor, "Too much", &[line(product, dec!(4))])
        .await
        .expect_err("more than sold");
    assert!(
        matches!(
            err,
            CreditNotesError::ExceedsReturnable { available, requested, .. }
                if available == dec!(3) && requested == dec!(4)
        ),
        "unexpected error: {err}"
    );

    assert_eq!(note_count(&pool, sale).await, 0);
    assert_eq!(return_movement_count(&pool, product).await, 0);
    assert_eq!(current_stock(&pool, product).await, dec!(7));
    assert_eq!(balance(&pool, wallet).await, dec!(4501.50));
}

#[sqlx::test]
async fn only_a_charged_sale_can_be_credited(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    // A draft has charged nothing: there is no money to give back.
    let draft = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .expect("item");

    let err = credit_notes::create(&pool, draft, actor, "Draft", &[line(product, dec!(1))])
        .await
        .expect_err("draft");
    assert!(matches!(err, CreditNotesError::SaleNotCreditable));
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(note_count(&pool, draft).await, 0);

    // A cancelled sale never charged either.
    sales::cancel(&pool, draft).await.expect("cancel");
    let err = credit_notes::create(&pool, draft, actor, "Cancelled", &[line(product, dec!(1))])
        .await
        .expect_err("cancelled");
    assert!(matches!(err, CreditNotesError::SaleNotCreditable));
    assert_eq!(note_count(&pool, draft).await, 0);
}

#[sqlx::test]
async fn an_unknown_sale_is_not_found(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;

    let err = credit_notes::create(
        &pool,
        Uuid::now_v7(),
        actor,
        "Unknown",
        &[line(product, dec!(1))],
    )
    .await
    .expect_err("unknown sale");
    assert!(matches!(err, CreditNotesError::SaleNotFound));
}

#[sqlx::test]
async fn a_product_that_is_not_in_the_sale_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let sold = seed_product(&pool, "Flour", dec!(100)).await;
    let other = seed_product(&pool, "Sugar", dec!(50)).await;
    stock(&pool, actor, sold, dec!(10)).await;
    stock(&pool, actor, other, dec!(10)).await;

    let (sale, method, _) = charged_sale(&pool, actor, sold, dec!(2)).await;
    let wallet = wallet_of(&pool, method).await;

    let err = credit_notes::create(&pool, sale, actor, "Wrong product", &[line(other, dec!(1))])
        .await
        .expect_err("not a line of the sale");
    assert!(matches!(err, CreditNotesError::ProductNotInSale));

    assert_eq!(note_count(&pool, sale).await, 0);
    assert_eq!(current_stock(&pool, other).await, dec!(10));
    assert_eq!(current_stock(&pool, sold).await, dec!(8));
    assert_eq!(balance(&pool, wallet).await, dec!(200.00));
}

#[sqlx::test]
async fn a_note_needs_a_reason_a_line_and_a_clean_quantity(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let one = ReturnLine {
        product_id: product,
        quantity: dec!(1),
    };
    let long_reason = "x".repeat(201);

    // Each case names the rule it breaks; every one must cost no write.
    let cases: Vec<(&str, Vec<ReturnLine>, CreditNotesError)> = vec![
        ("   ", vec![one], CreditNotesError::ReasonMissing),
        (
            long_reason.as_str(),
            vec![one],
            CreditNotesError::ReasonTooLong,
        ),
        ("No lines", Vec::new(), CreditNotesError::Empty),
        (
            "Zero",
            vec![line(product, Decimal::ZERO)],
            CreditNotesError::QuantityNotPositive,
        ),
        (
            "Too precise",
            vec![line(product, dec!(1.0005))],
            CreditNotesError::QuantityPrecision,
        ),
        ("Twice", vec![one, one], CreditNotesError::DuplicateProduct),
    ];

    for (reason, lines, expected) in cases {
        let err = credit_notes::create(&pool, sale, actor, reason, &lines)
            .await
            .expect_err("invalid request");
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&expected),
            "{reason:?} produced {err}"
        );
    }

    // Nothing was written by any of the six rejections.
    assert_eq!(note_count(&pool, sale).await, 0);
    assert_eq!(return_movement_count(&pool, product).await, 0);
    assert_eq!(current_stock(&pool, product).await, dec!(8));
}

// --- The money goes back the way it came ---------------------------------

#[sqlx::test]
async fn a_cash_and_card_sale_refunds_both_wallets_in_proportion(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let cash_method = cash(&pool).await;
    let cash_wallet = wallet_of(&pool, cash_method).await;

    let card_wallet = payments::create_wallet(
        &pool,
        CreateWalletInput {
            name: "Bank".to_string(),
        },
    )
    .await
    .expect("wallet");
    let card_method = payments::create_method(
        &pool,
        CreateMethodInput {
            name: "Card".to_string(),
            wallet_id: card_wallet,
        },
    )
    .await
    .expect("method");

    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, product, dec!(3))
        .await
        .expect("item");
    sales::complete(
        &pool,
        sale,
        actor,
        &[
            SalePaymentInput {
                payment_method_id: cash_method,
                amount: dec!(100),
            },
            SalePaymentInput {
                payment_method_id: card_method,
                amount: dec!(200),
            },
        ],
    )
    .await
    .expect("charge");

    // One unit comes back: a third of the money, in the same proportion it
    // entered. 33.33 leaves cash, 66.67 leaves the bank.
    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "One returned",
        &[line(product, dec!(1))],
    )
    .await
    .expect("credit note");

    assert_eq!(note.note.total, dec!(100.00));
    assert_eq!(note.refunds.len(), 2);
    assert_eq!(note.refunds[0].wallet_id, cash_wallet);
    assert_eq!(note.refunds[0].amount, dec!(33.33));
    assert_eq!(note.refunds[1].wallet_id, card_wallet);
    assert_eq!(note.refunds[1].amount, dec!(66.67));
    assert_eq!(
        note.refunds
            .iter()
            .map(|refund| refund.amount)
            .sum::<Decimal>(),
        dec!(100.00)
    );

    assert_eq!(balance(&pool, cash_wallet).await, dec!(66.67));
    assert_eq!(balance(&pool, card_wallet).await, dec!(133.33));
}

#[sqlx::test]
async fn a_refund_keeps_the_sale_and_the_method_on_the_ledger(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(2)).await;

    let note = credit_notes::create(&pool, sale, actor, "Returned", &[line(product, dec!(1))])
        .await
        .expect("credit note");

    let row = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>, Option<String>)>(
        "SELECT sale_id, payment_method_id, note FROM wallet_movements WHERE kind = 'sale_refund'",
    )
    .fetch_one(&pool)
    .await
    .expect("refund movement");

    assert_eq!(row.0, Some(sale));
    assert_eq!(row.1, Some(method));
    let label = row.2.expect("label");
    assert!(label.contains(&note.note.id.to_string()), "label: {label}");
    assert!(label.contains("Returned"), "label: {label}");
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
async fn http_credit_note_returns_the_document(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let cookie = login_cookie(&pool, &password).await;

    let response = app(pool.clone())
        .oneshot(
            with_session(
                "POST",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "reason": "Broken on arrival",
                    "items": [{ "product_id": product, "quantity": "1" }],
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["sale_id"], sale.to_string());
    assert_eq!(json["reason"], "Broken on arrival");
    assert_eq!(json["total"], "1500.50");
    assert_eq!(json["items"][0]["quantity"], "1");
    assert_eq!(json["items"][0]["unit_price"], "1500.50");
    assert_eq!(json["refunds"][0]["wallet_name"], "Caja");
    assert_eq!(json["refunds"][0]["amount"], "1500.50");

    // And the rejected case, over HTTP: more than was sold is a 409.
    let response = app(pool.clone())
        .oneshot(
            with_session(
                "POST",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "reason": "Too many",
                    "items": [{ "product_id": product, "quantity": "2" }],
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
}
