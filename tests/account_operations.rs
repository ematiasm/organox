//! Credit refunds, manual adjustments and the deactivation guard, over HTTP.
//!
//! Three rules from the customer-accounts design record (S10, S11, S12):
//!
//! - `POST /api/v1/customers/{id}/credit-refunds` hands collected credit back
//!   through an `account_payment_refund` wallet movement and a `credit_refund`
//!   ledger entry in one transaction, never letting the balance cross positive.
//! - `POST /api/v1/customers/{id}/adjustments` records an `adjustment_in` or
//!   `adjustment_out` without touching any wallet, never crossing zero.
//! - `POST /api/v1/customers/{id}/deactivate` (and `PATCH /api/v1/customers/{id}`
//!   with `is_active: false`) refuses to deactivate a customer while `balance != 0`
//!   or `unapplied_credit != 0`, naming the balance in a `409` conflict.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::customer_accounts::repository as ledger;
use organox::payments::repository as wallets;
use organox::routes::router;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

async fn seed_owner(pool: &PgPool) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse("owner@pyme.com").expect("email");
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

async fn login(router: &axum::Router, email: &str, password: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": password }).to_string(),
        ))
        .expect("login request");

    let res = router.clone().oneshot(req).await.expect("login response");
    assert_eq!(res.status(), StatusCode::OK);
    res.headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            let text = value.to_str().ok()?;
            text.starts_with("session=")
                .then(|| text.split(';').next().unwrap().to_string())
        })
        .expect("session cookie")
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let body = match body {
        Some(json_body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(json_body.to_string())
        }
        None => Body::empty(),
    };

    let res = router
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, parsed)
}

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

async fn charge_sale(pool: &PgPool, customer_id: Uuid, sale_id: Uuid, amount: Decimal) {
    let mut tx = pool.begin().await.expect("transaction");
    ledger::record_charge(&mut tx, customer_id, sale_id, amount, Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");
}

async fn seed_method(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
    let wallet = Uuid::now_v7();
    let method = Uuid::now_v7();
    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
        .bind(wallet)
        .bind(format!("{name}'s wallet"))
        .execute(pool)
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
        .bind(method)
        .bind(name)
        .bind(wallet)
        .execute(pool)
        .await
        .expect("method");
    (method, wallet)
}

async fn collect_unallocated(
    router: &axum::Router,
    cookie: &str,
    customer: Uuid,
    method: Uuid,
    amount: &str,
    idempotency_key: Option<&str>,
) {
    let mut payload = json!({
        "payment_method_id": method,
        "amount": amount,
        "allocations": [],
    });
    if let Some(key) = idempotency_key {
        payload["idempotency_key"] = json!(key);
    }
    let (status, _) = send(
        router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(cookie),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

async fn wallet_movement_count(pool: &PgPool, customer_id: Uuid, kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE customer_id = $1 AND kind::text = $2",
    )
    .bind(customer_id)
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("movement count")
}

async fn entry_count(pool: &PgPool, customer_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .expect("entry count")
}

async fn customer_is_active(pool: &PgPool, customer_id: Uuid) -> bool {
    sqlx::query_scalar("SELECT is_active FROM customers WHERE id = $1")
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .expect("is_active")
}

// --- S10: Credit refunds -------------------------------------------------

#[sqlx::test]
async fn refunding_credit_writes_the_outflow_and_moves_balance_toward_zero(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    collect_unallocated(&router, &cookie, customer, method, "120.00", None).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "50.00",
            "note": "customer asked for part of the advance back",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let refund_id: Uuid = body["refund_id"]
        .as_str()
        .expect("refund id")
        .parse()
        .expect("uuid");
    let movement_id: Uuid = body["wallet_movement_id"]
        .as_str()
        .expect("movement id")
        .parse()
        .expect("uuid");
    assert_eq!(body["balance"], "-70.00");
    assert_eq!(body["unapplied_credit"], "70.00");

    // The wallet took 120 in and handed 50 back out -> 70 left in the drawer.
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, dec!(70.00));

    // Exact wallet movement shape: account_payment_refund carrying customer and method,
    // no sale and no credit note.
    let movement: (
        String,
        Decimal,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT kind::text, amount, customer_id, payment_method_id, sale_id, credit_note_id
         FROM wallet_movements WHERE id = $1",
    )
    .bind(movement_id)
    .fetch_one(&pool)
    .await
    .expect("movement row");
    assert_eq!(movement.0, "account_payment_refund");
    assert_eq!(movement.1, dec!(50.00));
    assert_eq!(movement.2, Some(customer));
    assert_eq!(movement.3, Some(method));
    assert_eq!(movement.4, None);
    assert_eq!(movement.5, None);

    // Ledger entry matches the movement and preserves the S2 invariant.
    let entry: (String, Decimal, Option<Uuid>) = sqlx::query_as(
        "SELECT kind::text, amount, wallet_movement_id FROM customer_account_entries WHERE id = $1",
    )
    .bind(refund_id)
    .fetch_one(&pool)
    .await
    .expect("entry row");
    assert_eq!(entry.0, "credit_refund");
    assert_eq!(entry.1, dec!(50.00));
    assert_eq!(entry.2, Some(movement_id));

    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(-70.00));
    assert_eq!(position.unapplied_credit, dec!(70.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn credit_refund_refused_when_balance_would_turn_positive_leaves_no_trace(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // Unallocated receipt of 100 beside a 100 debt: U = 100, balance = 0.
    // Refunding even 10 would push balance to +10 (debt) -> must be refused.
    collect_unallocated(&router, &cookie, customer, method, "100.00", None).await;
    let entries_before = entry_count(&pool, customer).await;

    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "10.00",
            "note": "trying to cash out offset credit",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("0.00"),
        "detail should explain available credit is 0.00: {problem}"
    );
    assert_eq!(
        wallet_movement_count(&pool, customer, "account_payment_refund").await,
        0
    );
    assert_eq!(entry_count(&pool, customer).await, entries_before);
    assert_eq!(
        wallets::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(100.00)
    );
}

#[sqlx::test]
async fn replayed_credit_refund_returns_200_and_writes_no_second_movement(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    collect_unallocated(&router, &cookie, customer, method, "100.00", None).await;

    let body = json!({
        "idempotency_key": "refund-1",
        "payment_method_id": method,
        "amount": "40.00",
        "note": "return excess cash",
    });

    let (first_status, first) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(body.clone()),
    )
    .await;
    assert_eq!(first_status, StatusCode::CREATED);

    let (second_status, second) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(body),
    )
    .await;
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(second["refund_id"], first["refund_id"]);
    assert_eq!(second["wallet_movement_id"], first["wallet_movement_id"]);
    assert_eq!(second["balance"], "-60.00");
    assert_eq!(second["unapplied_credit"], "60.00");
    assert_eq!(
        wallet_movement_count(&pool, customer, "account_payment_refund").await,
        1
    );
    assert_eq!(
        wallets::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(60.00)
    );
}

#[sqlx::test]
async fn credit_refund_reusing_a_receipt_idempotency_key_answers_409_and_writes_nothing(
    pool: PgPool,
) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    collect_unallocated(
        &router,
        &cookie,
        customer,
        method,
        "100.00",
        Some("shared-key"),
    )
    .await;

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "shared-key",
            "payment_method_id": method,
            "amount": "20.00",
            "note": "collision with receipt key",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        wallet_movement_count(&pool, customer, "account_payment_refund").await,
        0
    );
    assert_eq!(
        wallets::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(100.00)
    );

    // Reverse direction: a receipt reusing a credit refund's idempotency key
    // must also answer 409 and write no account_payment movement instead of
    // replaying the refund as a receipt.
    let (refund_status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "refund-key",
            "payment_method_id": method,
            "amount": "20.00",
            "note": "refund first",
        })),
    )
    .await;
    assert_eq!(refund_status, StatusCode::CREATED);

    let (receipt_status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "refund-key",
            "payment_method_id": method,
            "amount": "15.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(receipt_status, StatusCode::CONFLICT);
    assert_eq!(
        wallet_movement_count(&pool, customer, "account_payment").await,
        1
    );
    assert_eq!(
        wallets::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(80.00)
    );
}

#[sqlx::test]
async fn credit_refund_validates_note_key_method_and_amount_precedence(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    collect_unallocated(&router, &cookie, customer, method, "100.00", None).await;

    // Blank note -> 422.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "10.00",
            "note": "   ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Overlong idempotency key -> 422.
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "x".repeat(101),
            "payment_method_id": method,
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        problem["detail"],
        "the idempotency key exceeds 100 characters"
    );

    // Non-positive amount -> 422.
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "0.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["detail"], "the amount must be positive");

    // Three decimals -> 422.
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "10.005",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        problem["detail"],
        "the amount 10.005 carries more than two decimals"
    );

    // Unknown payment method -> 422.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": Uuid::now_v7(),
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Inactive payment method -> 422.
    let (inactive_method, _) = seed_method(&pool, "Old Card").await;
    sqlx::query("UPDATE payment_methods SET is_active = false WHERE id = $1")
        .bind(inactive_method)
        .execute(&pool)
        .await
        .expect("deactivate method");
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": inactive_method,
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unauthenticated -> 401 on both new endpoints.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        None,
        Some(json!({
            "payment_method_id": method,
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        None,
        Some(json!({
            "direction": "in",
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Unknown customer -> 404.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{}/credit-refunds", Uuid::now_v7()),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "10.00",
            "note": "valid note",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(
        wallet_movement_count(&pool, customer, "account_payment_refund").await,
        0
    );
}

// --- S11: Manual adjustments ---------------------------------------------

#[sqlx::test]
async fn adjustment_in_loads_opening_balance_and_adjustment_out_clears_it(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // Opening balance: 0 -> 250.00 without writing any wallet movement.
    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "250.00",
            "note": "opening balance from paper ledger",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["adjustment_id"].as_str().is_some());
    assert_eq!(body["kind"], "adjustment_in");
    assert_eq!(body["balance"], "250.00");
    assert_eq!(body["unapplied_credit"], "0.00");

    let movements: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallet_movements")
        .fetch_one(&pool)
        .await
        .expect("movement count");
    assert_eq!(movements, 0, "adjustments never touch wallets");

    // Decrease down to zero: 250.00 -> 0.00.
    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "out",
            "amount": "250.00",
            "note": "debt forgiven by owner",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["kind"], "adjustment_out");
    assert_eq!(body["balance"], "0.00");
    assert_eq!(body["unapplied_credit"], "0.00");

    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, Decimal::ZERO);
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn adjustment_cannot_cross_zero_in_either_direction(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // On a zero balance, adjustment_out would mint credit -> refused 422.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "out",
            "amount": "10.00",
            "note": "trying to mint credit",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(entry_count(&pool, customer).await, 0);

    // Give the customer credit (balance = -50.00): neither `in` nor `out` may run.
    collect_unallocated(&router, &cookie, customer, method, "50.00", None).await;
    let before = entry_count(&pool, customer).await;

    for direction in ["in", "out"] {
        let (status, _) = send(
            &router,
            "POST",
            &format!("/api/v1/customers/{customer}/adjustments"),
            Some(&cookie),
            Some(json!({
                "direction": direction,
                "amount": "10.00",
                "note": "adjusting while in credit",
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "direction {direction} must be refused while balance < 0"
        );
    }
    assert_eq!(entry_count(&pool, customer).await, before);
}

#[sqlx::test]
async fn adjustment_requires_non_blank_note_and_valid_amount(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "50.00",
            "note": "   ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["detail"], "an adjustment needs a reason");

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "1000000000000.00",
            "note": "too big for column",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{}/adjustments", Uuid::now_v7()),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "10.00",
            "note": "missing customer",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(entry_count(&pool, customer).await, 0);
}

// --- S12: Deactivation guard ---------------------------------------------

#[sqlx::test]
async fn cannot_deactivate_customer_with_debt_or_credit_until_settled(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // 1. Positive balance (debt = 100.00) -> 409 naming the balance, stays active.
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("100.00"),
        "conflict detail must name the balance: {problem}"
    );
    assert!(customer_is_active(&pool, customer).await);

    // PATCH with is_active: false enforces the exact same guard.
    let (status, problem) = send(
        &router,
        "PATCH",
        &format!("/api/v1/customers/{customer}"),
        Some(&cookie),
        Some(json!({ "is_active": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("100.00"),
        "PATCH deactivation conflict must name the balance: {problem}"
    );
    assert!(customer_is_active(&pool, customer).await);

    // Editing another field while carrying a balance still works.
    let (status, _) = send(
        &router,
        "PATCH",
        &format!("/api/v1/customers/{customer}"),
        Some(&cookie),
        Some(json!({ "name": "Ana Renamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 2. Balance = 0.00 with U = 100.00 (unallocated receipt equal to debt) -> still 409!
    collect_unallocated(&router, &cookie, customer, method, "100.00", None).await;
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let detail = problem["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("0.00") && detail.contains("100.00"),
        "zero balance with unapplied credit must name both: {problem}"
    );
    assert!(customer_is_active(&pool, customer).await);

    // 3. Allocate the receipt to the sale: balance = 0, U = 0 -> deactivate succeeds!
    let receipt_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM customer_account_entries WHERE customer_id = $1 AND kind = 'receipt'",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .expect("receipt id");
    let mut tx = pool.begin().await.expect("tx");
    ledger::allocate(&mut tx, receipt_id, sale, dec!(100.00))
        .await
        .expect("allocate");
    tx.commit().await.expect("commit");

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!customer_is_active(&pool, customer).await);
}

#[sqlx::test]
async fn cannot_deactivate_customer_holding_credit_until_refunded(pool: PgPool) {
    let customer = seed_customer(&pool, "Beto").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    collect_unallocated(&router, &cookie, customer, method, "40.00", None).await;

    // Negative balance (-40.00) -> 409 naming -40.00.
    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("-40.00"),
        "conflict detail must name negative balance: {problem}"
    );
    assert!(customer_is_active(&pool, customer).await);

    // Refund the full 40.00 -> balance = 0, U = 0 -> deactivate now succeeds!
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "40.00",
            "note": "closing account",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!customer_is_active(&pool, customer).await);
}
