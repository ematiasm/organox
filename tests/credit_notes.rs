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
use organox::customer_accounts::repository as ledger;
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

// --- Reading a sale's credits ------------------------------------------

#[sqlx::test]
async fn a_charged_sale_with_no_returns_has_every_line_returnable(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let (sale, _, total) = charged_sale(&pool, actor, product, dec!(3)).await;

    let summary = credit_notes::for_sale(&pool, sale)
        .await
        .expect("credit summary");

    assert_eq!(summary.sale_id, sale);
    assert_eq!(summary.sale_total, total);
    assert_eq!(summary.refunded_total, Decimal::ZERO);
    assert!(summary.notes.is_empty());
    assert_eq!(summary.lines.len(), 1);
    assert_eq!(summary.lines[0].product_id, product);
    assert_eq!(summary.lines[0].product_name, "Flour");
    assert_eq!(summary.lines[0].quantity, dec!(3));
    assert_eq!(summary.lines[0].unit_price, dec!(1500.50));
    assert_eq!(summary.lines[0].credited_quantity, Decimal::ZERO);
    assert_eq!(summary.lines[0].returnable_quantity(), dec!(3));
}

#[sqlx::test]
async fn reading_a_sale_after_a_partial_note_shows_the_note_its_refund_and_what_is_left(
    pool: PgPool,
) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let (sale, method, total) = charged_sale(&pool, actor, product, dec!(3)).await;
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

    let summary = credit_notes::for_sale(&pool, sale)
        .await
        .expect("credit summary");

    assert_eq!(summary.sale_total, total);
    assert_eq!(summary.refunded_total, note.note.total);
    assert_eq!(summary.lines[0].credited_quantity, dec!(1));
    assert_eq!(summary.lines[0].returnable_quantity(), dec!(2));

    assert_eq!(summary.notes.len(), 1);
    let detail = &summary.notes[0];
    assert_eq!(detail.note.id, note.note.id);
    assert_eq!(detail.note.reason, "One was damaged");
    assert_eq!(detail.note.total, dec!(1500.50));
    assert_eq!(detail.items.len(), 1);
    assert_eq!(detail.items[0].product_name, "Flour");
    assert_eq!(detail.items[0].quantity, dec!(1));
    assert_eq!(detail.items[0].unit_price, dec!(1500.50));
    assert_eq!(detail.refunds.len(), 1);
    assert_eq!(detail.refunds[0].wallet_id, wallet);
    assert_eq!(detail.refunds[0].wallet_name, "Caja");
    assert_eq!(detail.refunds[0].payment_method_name, "Cash");
    assert_eq!(detail.refunds[0].amount, dec!(1500.50));
}

#[sqlx::test]
async fn notes_and_their_items_are_read_in_a_stable_order(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let zucchini = seed_product(&pool, "Zucchini", dec!(10)).await;
    let apple = seed_product(&pool, "Apple", dec!(20)).await;
    stock(&pool, actor, zucchini, dec!(10)).await;
    stock(&pool, actor, apple, dec!(10)).await;

    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, zucchini, dec!(2))
        .await
        .expect("zucchini");
    sales::add_item(&pool, sale, apple, dec!(2))
        .await
        .expect("apple");
    let total = sales::get(&pool, sale).await.expect("sale").total();
    sales::complete(
        &pool,
        sale,
        actor,
        &[SalePaymentInput {
            payment_method_id: cash(&pool).await,
            amount: total,
        }],
    )
    .await
    .expect("charge");

    for reason in ["First visit", "Second visit"] {
        credit_notes::create(
            &pool,
            sale,
            actor,
            reason,
            &[line(zucchini, dec!(1)), line(apple, dec!(1))],
        )
        .await
        .expect("credit note");
    }

    let summary = credit_notes::for_sale(&pool, sale)
        .await
        .expect("credit summary");
    let reasons: Vec<&str> = summary
        .notes
        .iter()
        .map(|detail| detail.note.reason.as_str())
        .collect();
    assert_eq!(reasons, vec!["First visit", "Second visit"]);
    // `credit_note_items` has no timestamp: product name is the stable order.
    for detail in &summary.notes {
        let names: Vec<&str> = detail
            .items
            .iter()
            .map(|item| item.product_name.as_str())
            .collect();
        assert_eq!(names, vec!["Apple", "Zucchini"]);
    }
    assert_eq!(summary.refunded_total, dec!(60.00));
}

#[sqlx::test]
async fn a_sale_without_a_stored_total_reports_the_computed_lines_total(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    let draft = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, draft, product, dec!(2.5))
        .await
        .expect("item");
    // A draft need not have a frozen total yet.
    sqlx::query("UPDATE sales SET total = NULL WHERE id = $1")
        .bind(draft)
        .execute(&pool)
        .await
        .expect("clear the stored total");

    let summary = credit_notes::for_sale(&pool, draft)
        .await
        .expect("credit summary");
    assert_eq!(summary.sale_total, dec!(3751.25));
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
async fn reading_an_unknown_sale_is_not_found(pool: PgPool) {
    let err = credit_notes::for_sale(&pool, Uuid::now_v7())
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

// --- The refund link ----------------------------------------------------

#[sqlx::test]
async fn every_refund_movement_names_the_note_that_caused_it(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let cash_method = cash(&pool).await;
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
    sales::add_item(&pool, sale, product, dec!(2))
        .await
        .expect("item");
    sales::complete(
        &pool,
        sale,
        actor,
        &[
            SalePaymentInput {
                payment_method_id: cash_method,
                amount: dec!(50),
            },
            SalePaymentInput {
                payment_method_id: card_method,
                amount: dec!(150),
            },
        ],
    )
    .await
    .expect("charge");

    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "One returned",
        &[line(product, dec!(1))],
    )
    .await
    .expect("credit note");

    let links: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT credit_note_id FROM wallet_movements WHERE kind = 'sale_refund'",
    )
    .fetch_all(&pool)
    .await
    .expect("refund links");
    assert_eq!(links, vec![Some(note.note.id), Some(note.note.id)]);

    let payments_with_a_note: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM wallet_movements
         WHERE kind = 'sale_payment' AND credit_note_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(payments_with_a_note, 0);
}

#[sqlx::test]
async fn a_sale_refund_without_a_note_is_rejected_by_the_database(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(2)).await;
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(1)).await;
    let wallet = wallet_of(&pool, method).await;

    // Raw SQL on purpose: no repository layer may add the link for us.
    let err = sqlx::query(
        "INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, sale_id, payment_method_id)
         VALUES ($1, $2, 'sale_refund', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(wallet)
    .bind(dec!(10.00))
    .bind(sale)
    .bind(method)
    .execute(&pool)
    .await
    .expect_err("a refund without a note");

    let db_err = err.as_database_error().expect("database error");
    assert_eq!(
        db_err.constraint(),
        Some("wallet_movements_sale_refund_names_its_note")
    );
}

#[sqlx::test]
async fn only_a_sale_refund_may_name_a_credit_note(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(2)).await;
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(1)).await;
    let wallet = wallet_of(&pool, method).await;
    let note = credit_notes::create(&pool, sale, actor, "Returned", &[line(product, dec!(1))])
        .await
        .expect("credit note");

    let err = sqlx::query(
        "INSERT INTO wallet_movements
            (id, wallet_id, kind, amount, credit_note_id)
         VALUES ($1, $2, 'expense', $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(wallet)
    .bind(dec!(10.00))
    .bind(note.note.id)
    .execute(&pool)
    .await
    .expect_err("an expense naming a note");

    let db_err = err.as_database_error().expect("database error");
    assert_eq!(
        db_err.constraint(),
        Some("wallet_movements_credit_note_only_for_sale_refunds")
    );
}

// --- HTTP ---------------------------------------------------------------

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

#[sqlx::test]
async fn http_get_credit_notes_returns_the_sale_credit_summary(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(3)).await;
    let cookie = login_cookie(&pool, &password).await;
    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "Broken on arrival",
        &[line(product, dec!(1))],
    )
    .await
    .expect("credit note");

    let response = app(pool.clone())
        .oneshot(
            with_session(
                "GET",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["sale_id"], sale.to_string());
    assert_eq!(json["sale_total"], "4501.50");
    assert_eq!(json["refunded_total"], "1500.50");
    assert_eq!(json["items"][0]["product_id"], product.to_string());
    assert_eq!(json["items"][0]["product_name"], "Flour");
    assert_eq!(json["items"][0]["quantity"], "3");
    assert_eq!(json["items"][0]["unit_price"], "1500.50");
    assert_eq!(json["items"][0]["credited_quantity"], "1");
    assert_eq!(json["items"][0]["returnable_quantity"], "2");
    assert_eq!(json["credit_notes"].as_array().unwrap().len(), 1);
    assert_eq!(json["credit_notes"][0]["id"], note.note.id.to_string());
    assert_eq!(json["credit_notes"][0]["reason"], "Broken on arrival");
    assert_eq!(json["credit_notes"][0]["total"], "1500.50");
    assert!(json["credit_notes"][0]["created_at"].is_string());
    assert_eq!(json["credit_notes"][0]["items"][0]["quantity"], "1");
    assert_eq!(json["credit_notes"][0]["refunds"][0]["wallet_name"], "Caja");
    assert_eq!(json["credit_notes"][0]["refunds"][0]["amount"], "1500.50");

    let response = app(pool.clone())
        .oneshot(
            with_session(
                "GET",
                &format!("/api/v1/sales/{}/credit-notes", Uuid::now_v7()),
                &cookie,
            )
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

// --- Review follow-ups --------------------------------------------------

#[sqlx::test]
async fn concurrent_creates_for_the_last_unit_commit_exactly_once(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(1)).await;
    let wallet = wallet_of(&pool, method).await;

    let first_line = [line(product, dec!(1))];
    let second_line = [line(product, dec!(1))];
    let first = credit_notes::create(&pool, sale, actor, "First cashier", &first_line);
    let second = credit_notes::create(&pool, sale, actor, "Second cashier", &second_line);
    let (first, second) = tokio::join!(first, second);

    let mut issued = Vec::new();
    for result in [first, second] {
        match result {
            Ok(note) => issued.push(note),
            Err(CreditNotesError::ExceedsReturnable { available, .. }) => {
                assert_eq!(available, Decimal::ZERO);
            }
            Err(err) => panic!("unexpected error: {err}"),
        }
    }
    assert_eq!(issued.len(), 1);

    assert_eq!(note_count(&pool, sale).await, 1);
    assert_eq!(return_movement_count(&pool, product).await, 1);
    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE kind = 'sale_refund' AND sale_id = $1",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 1);
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(balance(&pool, wallet).await, Decimal::ZERO);
}

#[sqlx::test]
async fn a_fractional_quantity_returns_a_consistent_amount(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(3.00)).await;
    stock(&pool, actor, product, dec!(1)).await;

    // 0.333 * 3.00 = 0.999, rounded to the cent the sale charges 1.00.
    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, product, dec!(0.333))
        .await
        .expect("item");
    sales::complete(
        &pool,
        sale,
        actor,
        &[SalePaymentInput {
            payment_method_id: cash(&pool).await,
            amount: dec!(1.00),
        }],
    )
    .await
    .expect("charge");

    let note = credit_notes::create(
        &pool,
        sale,
        actor,
        "Fractional return",
        &[line(product, dec!(0.333))],
    )
    .await
    .expect("credit note");

    let refunded = note
        .refunds
        .iter()
        .map(|refund| refund.amount)
        .sum::<Decimal>();
    assert_eq!(refunded, note.note.total);
    assert_eq!(format!("{:.2}", note.note.total), "1.00");
    assert_eq!(format!("{:.2}", refunded), "1.00");
    assert_eq!(format!("{:.2}", note.items[0].subtotal()), "1.00");
    assert_eq!(current_stock(&pool, product).await, dec!(1));
}

#[sqlx::test]
async fn http_credit_note_reports_not_found_and_unprocessable(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let cookie = login_cookie(&pool, &password).await;

    let valid_line = serde_json::json!({ "product_id": product, "quantity": "1" });
    let cases = [
        (
            format!("/api/v1/sales/{}/credit-notes", Uuid::now_v7()),
            serde_json::json!({ "reason": "Broken", "items": [valid_line.clone()] }),
            axum::http::StatusCode::NOT_FOUND,
        ),
        (
            format!("/api/v1/sales/{sale}/credit-notes"),
            serde_json::json!({ "reason": "   ", "items": [valid_line] }),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("/api/v1/sales/{sale}/credit-notes"),
            serde_json::json!({
                "reason": "Broken",
                "items": [{ "product_id": product, "quantity": "0" }]
            }),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];

    for (uri, body, expected) in cases {
        let response = app(pool.clone())
            .oneshot(
                with_session("POST", &uri, &cookie)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{uri} -> {body}");
    }
    assert_eq!(note_count(&pool, sale).await, 0);
}

#[sqlx::test]
async fn the_cumulative_refund_cannot_exceed_the_sale_total(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(33.33)).await;
    stock(&pool, actor, product, dec!(1)).await;

    // 0.02 * 33.33 = 0.6666, rounded to the cent the sale charges 0.67.
    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, product, dec!(0.02))
        .await
        .expect("item");
    let method = cash(&pool).await;
    sales::complete(
        &pool,
        sale,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(0.67),
        }],
    )
    .await
    .expect("charge");
    let wallet = wallet_of(&pool, method).await;
    let starting_balance = balance(&pool, wallet).await;

    // Each 0.002-unit note is worth 0.07 after rounding: per-note rounding
    // could push the refunds past what the sale charged, and the cap must be
    // the wall. Nine notes refund 0.63; a tenth would reach 0.70 on a 0.67
    // sale. The loop stays bounded no matter what the numbers become.
    let mut refunded = Decimal::ZERO;
    let mut accepted = 0;
    while refunded + dec!(0.07) <= dec!(0.67) && accepted < 10 {
        let note = credit_notes::create(
            &pool,
            sale,
            actor,
            "Piece by piece",
            &[line(product, dec!(0.002))],
        )
        .await
        .expect("still under the sale total");
        refunded += note.note.total;
        accepted += 1;
    }
    assert_eq!(accepted, 9);
    assert_eq!(refunded, dec!(0.63));

    let stock_before_rejection = current_stock(&pool, product).await;

    // The note that would cross the line is refused by name.
    let err = credit_notes::create(
        &pool,
        sale,
        actor,
        "One too many",
        &[line(product, dec!(0.002))],
    )
    .await
    .expect_err("over the sale total");
    assert!(
        matches!(err, CreditNotesError::RefundExceedsSale { .. }),
        "unexpected error: {err}"
    );

    // Over HTTP the same rejection is a 409 whose detail names the amounts.
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
                    "reason": "One too many",
                    "items": [{ "product_id": product, "quantity": "0.002" }],
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        problem["detail"],
        "the returns add up to 0.70 for a sale of 0.67: \
         only 0.04 is left to refund"
    );

    // The rejection wrote nothing, and the wallet never went below zero.
    assert_eq!(note_count(&pool, sale).await, 9);
    assert_eq!(current_stock(&pool, product).await, stock_before_rejection);
    assert_eq!(balance(&pool, wallet).await, starting_balance - refunded);
    assert!(balance(&pool, wallet).await >= Decimal::ZERO);
}

#[sqlx::test]
async fn repeating_the_idempotency_key_returns_the_same_note(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let wallet = wallet_of(&pool, method).await;
    let charged = balance(&pool, wallet).await;

    let first = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Customer changed their mind",
        &[line(product, dec!(1))],
        Some("retry-1"),
    )
    .await
    .expect("first note");
    assert!(!first.replayed);

    // The retry is not a second return: the first note comes back untouched.
    let second = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Customer changed their mind",
        &[line(product, dec!(1))],
        Some("retry-1"),
    )
    .await
    .expect("the same note");
    assert!(second.replayed);
    assert_eq!(second.note.id, first.note.id);
    assert_eq!(second.note.total, first.note.total);

    assert_eq!(note_count(&pool, sale).await, 1);
    assert_eq!(return_movement_count(&pool, product).await, 1);
    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE kind = 'sale_refund' AND sale_id = $1",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 1);
    assert_eq!(balance(&pool, wallet).await, charged - first.note.total);

    // A different key is a different request: a fresh note.
    let third = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Damaged after all",
        &[line(product, dec!(1))],
        Some("retry-2"),
    )
    .await
    .expect("second note");
    assert!(!third.replayed);
    assert_ne!(third.note.id, first.note.id);
    assert_eq!(note_count(&pool, sale).await, 2);

    // A blank key is not an identity: rejected before anything is written.
    let blank = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Blank",
        &[line(product, dec!(1))],
        Some("   "),
    )
    .await
    .expect_err("blank key");
    assert!(matches!(blank, CreditNotesError::IdempotencyKeyMissing));

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
                    "reason": "Blank",
                    "items": [{ "product_id": product, "quantity": "1" }],
                    "idempotency_key": "   "
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

// --- Slice-6 follow-ups -------------------------------------------------

#[sqlx::test]
async fn a_keyed_create_answers_201_then_200_over_http(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let cookie = login_cookie(&pool, &password).await;

    let body = serde_json::json!({
        "reason": "Customer changed their mind",
        "items": [{ "product_id": product, "quantity": "1" }],
        "idempotency_key": "retry-201",
    })
    .to_string();

    // The first create issues the document: 201.
    let response = app(pool.clone())
        .oneshot(
            with_session(
                "POST",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.clone()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let first: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(first["id"].is_string(), "no note id in {first}");

    // The retry is not a second document: 200 with the same id, so a client
    // that lost the first response can safely resend.
    let response = app(pool.clone())
        .oneshot(
            with_session(
                "POST",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let replay: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    assert_eq!(replay["id"], first["id"]);
    assert_eq!(note_count(&pool, sale).await, 1);
}

#[sqlx::test]
async fn an_over_long_key_is_rejected(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;

    // 100 characters is an identity; 101 is noise, and a rejected key costs
    // no query and no write.
    let err = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Over-long",
        &[line(product, dec!(1))],
        Some(&"k".repeat(101)),
    )
    .await
    .expect_err("over-long key");
    assert!(matches!(err, CreditNotesError::IdempotencyKeyTooLong));
    assert_eq!(note_count(&pool, sale).await, 0);

    // Over HTTP the same rejection is a 422.
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
                    "reason": "Over-long",
                    "items": [{ "product_id": product, "quantity": "1" }],
                    "idempotency_key": "k".repeat(101),
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(note_count(&pool, sale).await, 0);
}

#[sqlx::test]
async fn two_concurrent_keyed_creates_write_one_note(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;
    // Slack: three units sold, one comes back, so the loser of the race is a
    // replay, never a quantity conflict.
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(3)).await;
    let wallet = wallet_of(&pool, method).await;
    let charged = balance(&pool, wallet).await;

    let first_line = [line(product, dec!(1))];
    let second_line = [line(product, dec!(1))];
    let first = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Two cashiers, one key",
        &first_line,
        Some("race-key"),
    );
    let second = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Two cashiers, one key",
        &second_line,
        Some("race-key"),
    );
    let (first, second) = tokio::join!(first, second);

    // Both callers get an answer, and it is the same document.
    let first = first.expect("first create");
    let second = second.expect("second create");
    assert_eq!(first.note.id, second.note.id);
    let fresh = [first, second].iter().filter(|note| !note.replayed).count();
    assert_eq!(fresh, 1, "exactly one of the two writes the note");

    // One note row, one stock movement, one refund row: the money moved once.
    assert_eq!(note_count(&pool, sale).await, 1);
    assert_eq!(return_movement_count(&pool, product).await, 1);
    let refunds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE kind = 'sale_refund' AND sale_id = $1",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("refund count");
    assert_eq!(refunds, 1);
    assert_eq!(balance(&pool, wallet).await, charged - dec!(100.00));
}

#[sqlx::test]
async fn a_fractional_sale_discloses_the_money_allowance(pool: PgPool) {
    use tower::ServiceExt;

    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(33.33)).await;
    stock(&pool, actor, product, dec!(1)).await;

    // 0.02 units at 33.33 charge 0.67 after cent rounding.
    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, product, dec!(0.02))
        .await
        .expect("item");
    let method = cash(&pool).await;
    sales::complete(
        &pool,
        sale,
        actor,
        &[SalePaymentInput {
            payment_method_id: method,
            amount: dec!(0.67),
        }],
    )
    .await
    .expect("charge");
    let wallet = wallet_of(&pool, method).await;
    let charged = balance(&pool, wallet).await;

    // Each 0.002-unit note refunds 0.07 after rounding. Issue them until the
    // next one would cross the money cap: nine land, 0.63 out of 0.67.
    let mut refunded = Decimal::ZERO;
    let mut accepted = 0;
    while refunded + dec!(0.07) <= dec!(0.67) && accepted < 10 {
        credit_notes::create(
            &pool,
            sale,
            actor,
            "Piece by piece",
            &[line(product, dec!(0.002))],
        )
        .await
        .expect("still under the sale total");
        refunded += dec!(0.07);
        accepted += 1;
    }
    assert_eq!(accepted, 9);
    assert_eq!(refunded, dec!(0.63));

    // The read endpoint discloses the same allowance create enforces — and
    // the quantity budget is NOT exhausted: 0.002 units remain returnable
    // against 0.04 of money, while the natural next note is worth 0.07.
    let summary = credit_notes::for_sale(&pool, sale)
        .await
        .expect("credit summary");
    assert_eq!(summary.sale_total, dec!(0.67));
    assert_eq!(summary.refunded_total, dec!(0.63));
    assert_eq!(summary.refundable_total, dec!(0.04));
    assert_eq!(summary.lines[0].returnable_quantity(), dec!(0.002));

    let cookie = login_cookie(&pool, &password).await;
    let response = app(pool.clone())
        .oneshot(
            with_session(
                "GET",
                &format!("/api/v1/sales/{sale}/credit-notes"),
                &cookie,
            )
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json["sale_total"], "0.67");
    assert_eq!(json["refunded_total"], "0.63");
    assert_eq!(json["refundable_total"], "0.04");

    let stock_before = current_stock(&pool, product).await;
    let balance_before = balance(&pool, wallet).await;

    // The note that crosses the allowance is refused, by name, and writes
    // neither stock nor money.
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
                    "reason": "One too many",
                    "items": [{ "product_id": product, "quantity": "0.002" }],
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let problem: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        problem["detail"],
        "the returns add up to 0.70 for a sale of 0.67: only 0.04 is left to refund"
    );

    assert_eq!(note_count(&pool, sale).await, 9);
    assert_eq!(current_stock(&pool, product).await, stock_before);
    assert_eq!(balance(&pool, wallet).await, balance_before);
    assert_eq!(balance(&pool, wallet).await, dec!(0.04));
    assert_eq!(charged - refunded, dec!(0.04));
}

// --- T9: the T8 advisories ----------------------------------------------

#[sqlx::test]
async fn a_key_of_exactly_one_hundred_characters_is_accepted_and_replays(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    let (sale, _, _) = charged_sale(&pool, actor, product, dec!(2)).await;

    // 100 characters is the boundary of an identity, and the server trims
    // before it measures: padding is not part of the length and not part of
    // the identity.
    let exact = "k".repeat(100);
    let padded = format!(" {exact} ");

    let first = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Boundary key",
        &[line(product, dec!(1))],
        Some(&exact),
    )
    .await
    .expect("a 100-character key is accepted");
    assert!(!first.replayed);

    // The retry with the same key replays: the same note, not a second one.
    let second = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Boundary key",
        &[line(product, dec!(1))],
        Some(&exact),
    )
    .await
    .expect("the boundary key replays");
    assert!(second.replayed);
    assert_eq!(second.note.id, first.note.id);

    // A key with padding that trims to 100 characters is the same identity:
    // accepted, never measured as a 102-character over-long key.
    let third = credit_notes::create_with_idempotency(
        &pool,
        sale,
        actor,
        "Boundary key",
        &[line(product, dec!(1))],
        Some(&padded),
    )
    .await
    .expect("the padded key is trimmed, not rejected");
    assert!(third.replayed);
    assert_eq!(third.note.id, first.note.id);

    // One note, one return: none of the retries wrote anything.
    assert_eq!(note_count(&pool, sale).await, 1);
    assert_eq!(return_movement_count(&pool, product).await, 1);
}

#[sqlx::test]
async fn a_keyed_create_needs_one_pool_connection(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100)).await;
    stock(&pool, actor, product, dec!(5)).await;
    // Charge with the normal pool first: it may use as many connections as
    // it likes.
    let (sale, method, _) = charged_sale(&pool, actor, product, dec!(2)).await;
    let wallet = wallet_of(&pool, method).await;
    let charged = balance(&pool, wallet).await;

    // A pool that can hand out exactly one connection. A keyed create that
    // needed a second checkout while holding the first would stall here until
    // the acquire timeout; success under the default timeout is the proof.
    // No sleeps: the assertions themselves are the clock.
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("one-connection pool");

    let first = credit_notes::create_with_idempotency(
        &one,
        sale,
        actor,
        "One connection is enough",
        &[line(product, dec!(1))],
        Some("single-connection"),
    )
    .await
    .expect("a keyed create holds exactly one connection");
    assert!(!first.replayed);

    // The replay takes the same single-connection path: the pre-check reads
    // through the open transaction, not a second checkout.
    let second = credit_notes::create_with_idempotency(
        &one,
        sale,
        actor,
        "One connection is enough",
        &[line(product, dec!(1))],
        Some("single-connection"),
    )
    .await
    .expect("the replay needs no second connection");
    assert!(second.replayed);
    assert_eq!(second.note.id, first.note.id);

    one.close().await;

    // Both calls together wrote one note and moved the money once.
    assert_eq!(note_count(&pool, sale).await, 1);
    assert_eq!(return_movement_count(&pool, product).await, 1);
    assert_eq!(balance(&pool, wallet).await, charged - dec!(100.00));
}

// --- T6: debt is cancelled before cash is refunded ----------------------

/// A customer, the way the ledger's own tests seed one: the registry's rule
/// is not what these tests exercise.
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

/// One authenticated POST with a JSON body, over the real router.
async fn post_json(
    pool: &PgPool,
    cookie: &str,
    uri: String,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;

    let response = app(pool.clone())
        .oneshot(
            with_session("POST", &uri, cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Issue a credit note over HTTP: `quantity` units of `product` come back.
async fn note_http(
    pool: &PgPool,
    cookie: &str,
    sale: Uuid,
    product: Uuid,
    quantity: Decimal,
) -> (axum::http::StatusCode, serde_json::Value) {
    post_json(
        pool,
        cookie,
        format!("/api/v1/sales/{sale}/credit-notes"),
        serde_json::json!({
            "reason": "Customer changed their mind",
            "items": [{ "product_id": product, "quantity": quantity.to_string() }],
        }),
    )
    .await
}

/// Collect money against a customer's account over HTTP, optionally paying
/// one sale with it.
async fn collect_http(
    pool: &PgPool,
    cookie: &str,
    customer: Uuid,
    method: Uuid,
    amount: &str,
    allocation: Option<(Uuid, &str)>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let allocations: Vec<serde_json::Value> = allocation
        .map(|(sale_id, amount)| serde_json::json!({ "sale_id": sale_id, "amount": amount }))
        .into_iter()
        .collect();
    post_json(
        pool,
        cookie,
        format!("/api/v1/customers/{customer}/receipts"),
        serde_json::json!({
            "payment_method_id": method,
            "amount": amount,
            "allocations": allocations,
        }),
    )
    .await
}

/// A completed sale of `quantity` units of `product` to `customer`, with
/// `at_till` collected by `method` and the rest charged to the account.
/// No customer and `at_till == total` is the plain cash sale of every older
/// test; no method and `at_till == 0` is a sale taken wholly on account.
async fn sale_part_on_account(
    pool: &PgPool,
    actor: Uuid,
    product: Uuid,
    quantity: Decimal,
    customer: Option<Uuid>,
    method: Option<Uuid>,
    at_till: Decimal,
) -> Uuid {
    let sale = sales::open_draft(pool, actor).await.expect("draft");
    sales::add_item(pool, sale, product, quantity)
        .await
        .expect("item");
    if let Some(customer) = customer {
        sales::set_customer(pool, sale, Some(customer))
            .await
            .expect("customer");
    }
    let total = sales::get(pool, sale).await.expect("sale").total();
    let payments = method
        .map(|method| {
            vec![SalePaymentInput {
                payment_method_id: method,
                amount: at_till,
            }]
        })
        .unwrap_or_default();
    sales::complete_with_on_account(pool, sale, actor, &payments, total - at_till)
        .await
        .expect("charge");
    sale
}

/// How many `sale_refund` movements one sale has written.
async fn refund_count(pool: &PgPool, sale: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE kind = 'sale_refund' AND sale_id = $1",
    )
    .bind(sale)
    .fetch_one(pool)
    .await
    .expect("refund count")
}

#[sqlx::test]
async fn a_note_on_an_unpaid_credit_sale_only_cancels_debt(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let customer = seed_customer(&pool, "Ana").await;
    let product = seed_product(&pool, "Flour", dec!(10.00)).await;
    stock(&pool, actor, product, dec!(10)).await;

    // A sale of 100 taken entirely on account: nothing at the till, all debt.
    let sale = sale_part_on_account(
        &pool,
        actor,
        product,
        dec!(10),
        Some(customer),
        None,
        Decimal::ZERO,
    )
    .await;
    let cookie = login_cookie(&pool, &password).await;

    let (status, body) = note_http(&pool, &cookie, sale, product, dec!(6)).await;

    // The note is taken, and its whole value cancels debt: no wallet ever
    // collected any money for this sale, so none may hand any back.
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["total"], "60.00");
    assert!(
        body["refunds"].as_array().expect("refunds").is_empty(),
        "an unpaid credit sale refunded cash: {body}"
    );

    // The sale keeps owing the remaining 40 on the ledger's own definition.
    let owed = ledger::outstanding_by_sale(&pool, customer)
        .await
        .expect("outstanding");
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].sale_id, sale);
    assert_eq!(owed[0].outstanding, dec!(40.00));

    // No wallet movement was written at all.
    let movements: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallet_movements")
        .fetch_one(&pool)
        .await
        .expect("movement count");
    assert_eq!(movements, 0);

    // And the two books agree: 40 of debt, 40 of balance, nothing else.
    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(40.00));
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_note_cancels_debt_before_refunding_cash(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let customer = seed_customer(&pool, "Ana").await;
    let product = seed_product(&pool, "Flour", dec!(10.00)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let method = cash(&pool).await;
    let wallet = wallet_of(&pool, method).await;

    // 40 at the till, 60 on the account.
    let sale = sale_part_on_account(
        &pool,
        actor,
        product,
        dec!(10),
        Some(customer),
        Some(method),
        dec!(40.00),
    )
    .await;
    assert_eq!(balance(&pool, wallet).await, dec!(40.00));
    let cookie = login_cookie(&pool, &password).await;

    // A note of 80: the 60 of debt go first, only 20 leave the wallet.
    let (status, body) = note_http(&pool, &cookie, sale, product, dec!(8)).await;

    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["total"], "80.00");
    assert_eq!(body["refunds"].as_array().expect("refunds").len(), 1);
    assert_eq!(body["refunds"][0]["wallet_name"], "Caja");
    assert_eq!(body["refunds"][0]["amount"], "20.00");

    // The wallet that collected 40 gave back only the cash part of the note.
    assert_eq!(balance(&pool, wallet).await, dec!(20.00));

    // The debt is gone: the ledger carries the 60 as a cancellation.
    let owed = ledger::outstanding_by_sale(&pool, customer)
        .await
        .expect("outstanding");
    assert!(owed.is_empty());
    let cancelled: Decimal = sqlx::query_scalar!(
        r#"SELECT -amount AS "cancelled!: Decimal"
           FROM customer_account_entries
           WHERE sale_id = $1 AND kind = 'credit_note'"#,
        sale
    )
    .fetch_one(&pool)
    .await
    .expect("cancellation");
    assert_eq!(cancelled, dec!(60.00));

    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, Decimal::ZERO);
    assert!(position.is_consistent());

    // One refund movement through the till's own method, and nothing else.
    assert_eq!(refund_count(&pool, sale).await, 1);
}

#[sqlx::test]
async fn a_note_refunds_only_the_money_actually_collected(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let customer = seed_customer(&pool, "Ana").await;
    let product = seed_product(&pool, "Flour", dec!(10.00)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let cash_method = cash(&pool).await;
    let till_wallet = wallet_of(&pool, cash_method).await;
    let bank_wallet = payments::create_wallet(
        &pool,
        CreateWalletInput {
            name: "Bank".to_string(),
        },
    )
    .await
    .expect("wallet");
    let bank_method = payments::create_method(
        &pool,
        CreateMethodInput {
            name: "Card".to_string(),
            wallet_id: bank_wallet,
        },
    )
    .await
    .expect("method");

    // 40 at the till, 60 on the account.
    let sale = sale_part_on_account(
        &pool,
        actor,
        product,
        dec!(10),
        Some(customer),
        Some(cash_method),
        dec!(40.00),
    )
    .await;
    let cookie = login_cookie(&pool, &password).await;

    // The customer pays the 60 later, by a collection allocated to the sale.
    let (status, _) = collect_http(
        &pool,
        &cookie,
        customer,
        bank_method,
        "60.00",
        Some((sale, "60.00")),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(balance(&pool, bank_wallet).await, dec!(60.00));

    // A note for the whole sale: the collection already paid the debt, so
    // nothing is cancelled; the 100 is refunded by the tenders that
    // collected it — the till's 40 and the collection's 60.
    let (status, body) = note_http(&pool, &cookie, sale, product, dec!(10)).await;

    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["total"], "100.00");
    let refunds = body["refunds"].as_array().expect("refunds");
    assert_eq!(refunds.len(), 2);
    assert_eq!(refunds[0]["wallet_name"], "Caja");
    assert_eq!(refunds[0]["amount"], "40.00");
    assert_eq!(refunds[1]["wallet_name"], "Bank");
    assert_eq!(refunds[1]["amount"], "60.00");

    // Both wallets are back to zero: every cent went back the way it came.
    assert_eq!(balance(&pool, till_wallet).await, Decimal::ZERO);
    assert_eq!(balance(&pool, bank_wallet).await, Decimal::ZERO);

    // No debt was cancelled — the collection had already paid it — and no
    // stray movement exists beyond the two refunds.
    let owed = ledger::outstanding_by_sale(&pool, customer)
        .await
        .expect("outstanding");
    assert!(owed.is_empty());
    let cancellations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customer_account_entries
         WHERE sale_id = $1 AND kind = 'credit_note'",
    )
    .bind(sale)
    .fetch_one(&pool)
    .await
    .expect("cancellation count");
    assert_eq!(cancellations, 0);
    assert_eq!(refund_count(&pool, sale).await, 2);

    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_note_on_a_cash_sale_keeps_refunding_its_payments(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(100.00)).await;
    stock(&pool, actor, product, dec!(3)).await;
    let cash_method = cash(&pool).await;
    let till_wallet = wallet_of(&pool, cash_method).await;
    let bank_wallet = payments::create_wallet(
        &pool,
        CreateWalletInput {
            name: "Bank".to_string(),
        },
    )
    .await
    .expect("wallet");
    let bank_method = payments::create_method(
        &pool,
        CreateMethodInput {
            name: "Card".to_string(),
            wallet_id: bank_wallet,
        },
    )
    .await
    .expect("method");

    let sale = sales::open_draft(&pool, actor).await.expect("draft");
    sales::add_item(&pool, sale, product, dec!(3))
        .await
        .expect("item");
    let total = sales::get(&pool, sale).await.expect("sale").total();
    sales::complete(
        &pool,
        sale,
        actor,
        &[
            SalePaymentInput {
                payment_method_id: cash_method,
                amount: dec!(100.00),
            },
            SalePaymentInput {
                payment_method_id: bank_method,
                amount: dec!(200.00),
            },
        ],
    )
    .await
    .expect("charge");
    assert_eq!(total, dec!(300.00));

    let cookie = login_cookie(&pool, &password).await;
    let (status, body) = note_http(&pool, &cookie, sale, product, dec!(1)).await;

    // No customer, no account: the note is pure cash, split over the
    // payments in proportion, exactly as before the account existed.
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["refunds"][0]["amount"], "33.33");
    assert_eq!(body["refunds"][1]["amount"], "66.67");
    assert_eq!(balance(&pool, till_wallet).await, dec!(66.67));
    assert_eq!(balance(&pool, bank_wallet).await, dec!(133.33));
    assert_eq!(refund_count(&pool, sale).await, 2);
}

#[sqlx::test]
async fn three_notes_never_refund_more_than_the_sale_charged(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(10.00)).await;
    stock(&pool, actor, product, dec!(10)).await;
    let method = cash(&pool).await;
    let wallet = wallet_of(&pool, method).await;

    // A fully collected cash sale of 100.
    let sale = sale_part_on_account(
        &pool,
        actor,
        product,
        dec!(10),
        None,
        Some(method),
        dec!(100.00),
    )
    .await;
    let charged = balance(&pool, wallet).await;
    assert_eq!(charged, dec!(100.00));

    let cookie = login_cookie(&pool, &password).await;

    // Three notes of 33.33: each is the floor of a third of the money, and
    // the cumulative split must never invent the missing cent back.
    for _ in 0..3 {
        let (status, _) = note_http(&pool, &cookie, sale, product, dec!(3.333)).await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
    }

    assert_eq!(note_count(&pool, sale).await, 3);
    // 99.99 left the wallet, never 100.00.
    assert_eq!(balance(&pool, wallet).await, dec!(0.01));
    let refunded: Decimal = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(amount), 0) AS "refunded!: Decimal"
           FROM wallet_movements
           WHERE kind = 'sale_refund' AND sale_id = $1"#,
        sale
    )
    .fetch_one(&pool)
    .await
    .expect("refunded");
    assert_eq!(refunded, dec!(99.99));
}

#[sqlx::test]
async fn a_note_refuses_cash_no_tender_collected(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool).await;
    let product = seed_product(&pool, "Flour", dec!(10.00)).await;
    stock(&pool, actor, product, dec!(10)).await;

    // A completed sale whose money never reached any wallet: raw rows,
    // because the completion invariant cannot produce it — payments plus
    // on_account always equal the total, so a sale that still needs cash
    // has a tender somewhere. The refusal below is the guard for a ledger
    // that has drifted, and drift must be reported, not papered over.
    let sale = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO sales (id, status, total, created_by, completed_by, completed_at)
        VALUES ($1, 'completed', $2, $3, $3, now())
        "#,
    )
    .bind(sale)
    .bind(dec!(100.00))
    .bind(actor)
    .execute(&pool)
    .await
    .expect("sale");
    sqlx::query(
        "INSERT INTO sale_items (id, sale_id, product_id, quantity, unit_price)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(sale)
    .bind(product)
    .bind(dec!(10))
    .bind(dec!(10.00))
    .execute(&pool)
    .await
    .expect("sale item");

    let cookie = login_cookie(&pool, &password).await;
    let (status, _) = note_http(&pool, &cookie, sale, product, dec!(10)).await;

    // Refused whole, not silently short: cash no tender ever collected
    // cannot leave any wallet.
    assert_eq!(
        status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "a note owing cash with no tender was accepted"
    );
    assert_eq!(note_count(&pool, sale).await, 0);
    assert_eq!(refund_count(&pool, sale).await, 0);
    // The goods never moved either: the whole note was refused.
    assert_eq!(current_stock(&pool, product).await, dec!(10));
    assert_eq!(return_movement_count(&pool, product).await, 0);
}
