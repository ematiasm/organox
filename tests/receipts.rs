//! Collections against a customer's account, over HTTP.
//!
//! The endpoint this file pins is where money meets debt: one request writes
//! one `account_payment` wallet movement and one `receipt` ledger entry in the
//! same transaction, and every rejection must leave both books untouched. The
//! rules themselves (the ceiling per sale, the customer binding, the replay)
//! are also pinned at the ledger's own layer in `tests/customer_accounts.rs`;
//! what only HTTP can prove is the status codes, the JSON contract and the
//! 201/200 replay split.

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

/// A password generated per test run, so no credential is written down.
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

/// A completed sale with no items: enough to own a debt, nothing else. The
/// checkout path already has its own suite; what the collection needs is a
/// sale that owes money, as directly as possible.
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

/// Put a sale's unpaid part on the account, the way a checkout with
/// `on_account` does. The ledger writer is used rather than a raw insert so a
/// broken charge rule fails here loudly instead of producing a fixture that
/// no real flow could have written.
async fn charge_sale(pool: &PgPool, customer_id: Uuid, sale_id: Uuid, amount: Decimal) {
    let mut tx = pool.begin().await.expect("transaction");
    ledger::record_charge(&mut tx, customer_id, sale_id, amount, Uuid::now_v7())
        .await
        .expect("charge");
    tx.commit().await.expect("commit");
}

/// A wallet and an active method paying into it.
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

/// How many account payments one customer's collections have written: the
/// "nothing was written" assertion of every rejected request below.
async fn account_payment_count(pool: &PgPool, customer_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_movements WHERE kind = 'account_payment' AND customer_id = $1",
    )
    .bind(customer_id)
    .fetch_one(pool)
    .await
    .expect("movement count")
}

/// How many ledger entries one customer carries.
async fn entry_count(pool: &PgPool, customer_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM customer_account_entries WHERE customer_id = $1")
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .expect("entry count")
}

// --- The collection ------------------------------------------------------

#[sqlx::test]
async fn collecting_money_credits_the_wallet_and_lowers_the_debt(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "60.00",
            "allocations": [ { "sale_id": sale, "amount": "60.00" } ],
            "note": "first collection",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let receipt_id: Uuid = body["receipt_id"]
        .as_str()
        .expect("receipt id")
        .parse()
        .expect("uuid");
    let movement_id: Uuid = body["wallet_movement_id"]
        .as_str()
        .expect("movement id")
        .parse()
        .expect("uuid");

    // The debt went down by what was collected, and the sale keeps owing the
    // rest. Money with two decimals, as a string, like every other amount.
    assert_eq!(body["balance"], "40.00");
    assert_eq!(body["unapplied_credit"], "0.00");
    assert_eq!(body["allocations"][0]["sale_id"], sale.to_string());
    assert_eq!(body["allocations"][0]["outstanding"], "40.00");

    // The two books agree: the ledger says the same numbers the response did.
    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.balance, dec!(40.00));
    assert_eq!(position.unapplied_credit, Decimal::ZERO);
    assert_eq!(position.outstanding_total, dec!(40.00));
    assert!(position.is_consistent());

    // The wallet took the money, counted with the new kind's own sign.
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, dec!(60.00));

    // One account_payment movement carrying the customer, nothing else linked.
    let movement = sqlx::query_as::<
        _,
        (
            String,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
        ),
    >(
        r#"
        SELECT kind::text, customer_id, payment_method_id, sale_id, credit_note_id
        FROM wallet_movements WHERE id = $1
        "#,
    )
    .bind(movement_id)
    .fetch_one(&pool)
    .await
    .expect("movement");
    assert_eq!(movement.0, "account_payment");
    assert_eq!(
        movement.1,
        Some(customer),
        "the movement carries the customer"
    );
    assert_eq!(movement.2, Some(method));
    assert_eq!(movement.3, None, "an account payment names no sale");
    assert_eq!(movement.4, None, "an account payment names no credit note");

    // And one receipt entry linking back to that movement, money as signed.
    let entry = sqlx::query_as::<_, (Uuid, Decimal)>(
        "SELECT wallet_movement_id, amount FROM customer_account_entries WHERE id = $1",
    )
    .bind(receipt_id)
    .fetch_one(&pool)
    .await
    .expect("receipt entry");
    assert_eq!(entry.0, movement_id);
    assert_eq!(entry.1, dec!(-60.00));
}

#[sqlx::test]
async fn a_collection_can_leave_money_unapplied(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "150.00",
            "allocations": [ { "sale_id": sale, "amount": "60.00" } ],
        })),
    )
    .await;

    // The operator decides the allocations; the server never invents one. The
    // remainder is not a second entry anywhere — it is the usable credit.
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["balance"], "-50.00");
    assert_eq!(body["unapplied_credit"], "90.00");
    assert_eq!(body["allocations"][0]["outstanding"], "40.00");

    let position = ledger::account_position(&pool, customer)
        .await
        .expect("position");
    assert_eq!(position.unapplied_credit, dec!(90.00));
    assert!(position.is_consistent());
}

#[sqlx::test]
async fn a_replayed_collection_answers_200_and_writes_nothing(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let body = json!({
        "idempotency_key": "till-close-1",
        "payment_method_id": method,
        "amount": "60.00",
        "allocations": [ { "sale_id": sale, "amount": "60.00" } ],
    });

    let (fresh_status, fresh) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(body.clone()),
    )
    .await;
    assert_eq!(fresh_status, StatusCode::CREATED);

    // The same key again: the first receipt is the answer, 200 says it was
    // not created again, and the money moved once.
    let (replayed_status, replayed) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(body),
    )
    .await;
    assert_eq!(replayed_status, StatusCode::OK);
    assert_eq!(replayed["receipt_id"], fresh["receipt_id"]);

    assert_eq!(account_payment_count(&pool, customer).await, 1);
    assert_eq!(
        entry_count(&pool, customer).await,
        2,
        "the charge and one receipt"
    );

    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, dec!(60.00), "the replay collected twice");
}

#[sqlx::test]
async fn a_collection_cannot_allocate_more_than_the_sale_owes(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "100.00",
            "allocations": [ { "sale_id": sale, "amount": "150.00" } ],
        })),
    )
    .await;

    // The ceiling per sale, with the sentence an operator can act on.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["detail"],
        "the allocation of 150.00 exceeds the 100.00 this sale still owes"
    );

    // A rejected collection leaves no movement, no entry, no wallet change.
    assert_eq!(account_payment_count(&pool, customer).await, 0);
    assert_eq!(entry_count(&pool, customer).await, 1, "only the charge");
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, Decimal::ZERO);
}

#[sqlx::test]
async fn a_collection_cannot_be_made_with_an_inactive_method(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Retired card").await;

    sqlx::query("UPDATE payment_methods SET is_active = false WHERE id = $1")
        .bind(method)
        .execute(&pool)
        .await
        .expect("deactivate");

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "60.00",
            "allocations": [ { "sale_id": sale, "amount": "60.00" } ],
        })),
    )
    .await;

    // The same wording the checkout uses: one sentence for a method that is
    // unknown and one that was switched off.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "the payment method is unknown or inactive");

    assert_eq!(account_payment_count(&pool, customer).await, 0);
    assert_eq!(entry_count(&pool, customer).await, 1, "only the charge");
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, Decimal::ZERO);
}

#[sqlx::test]
async fn a_collection_for_an_unknown_customer_answers_404(pool: PgPool) {
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, _body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{}/receipts", Uuid::now_v7()),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "60.00",
            "allocations": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);

    let movements: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wallet_movements WHERE kind = 'account_payment'")
            .fetch_one(&pool)
            .await
            .expect("movement count");
    assert_eq!(movements, 0);
}

#[sqlx::test]
async fn a_collection_cannot_pay_another_customers_sale(pool: PgPool) {
    let ana = seed_customer(&pool, "Ana").await;
    let beto = seed_customer(&pool, "Beto").await;
    let beto_sale = seed_completed_sale(&pool, beto, dec!(100.00)).await;
    charge_sale(&pool, beto, beto_sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{ana}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "60.00",
            "allocations": [ { "sale_id": beto_sale, "amount": "60.00" } ],
        })),
    )
    .await;

    // Paying somebody else's sale with your money would move a debt between
    // two people, which is worse than losing the row.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "that sale belongs to another customer");

    assert_eq!(account_payment_count(&pool, ana).await, 0);
    assert_eq!(entry_count(&pool, ana).await, 0);
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, Decimal::ZERO);
}

/// `numeric(14, 2)` cannot hold more than this, and a value the column cannot
/// store must be a sentence, not a database overflow — the same lesson the
/// credit limit learned in review.
#[sqlx::test]
async fn an_amount_beyond_the_column_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method,
            "amount": "1000000000000.00",
            "allocations": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "the amount must be at most 999999999999.99");

    assert_eq!(account_payment_count(&pool, customer).await, 0);
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, Decimal::ZERO);
}

// --- HTTP helpers --------------------------------------------------------

/// Log in through the real route and keep the cookie header it sets.
async fn login(router: &axum::Router, email: &str, password: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "login must succeed");
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join("; ")
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
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// A blank key is no key. Accepting one would make every later blank-key
/// collection answer with the first receipt: money collected, nothing booked,
/// and a success reported for both.
#[sqlx::test]
async fn a_blank_idempotency_key_does_not_replay_the_first_collection(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    for attempt in 0..2 {
        let (status, _) = send(
            &router,
            "POST",
            &format!("/api/v1/customers/{customer}/receipts"),
            Some(&cookie),
            Some(json!({
                "idempotency_key": "   ",
                "payment_method_id": method,
                "amount": "10.00",
                "allocations": [],
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "collection {attempt} answered as a replay of the previous one"
        );
    }

    assert_eq!(
        account_payment_count(&pool, customer).await,
        2,
        "one of the two collections was swallowed as a replay"
    );
    let wallet_balance = wallets::get_wallet(&pool, wallet)
        .await
        .expect("wallet")
        .balance;
    assert_eq!(wallet_balance, dec!(20.00), "the cash did not arrive twice");
}

/// The contract publishes a 100-character cap. utoipa attributes do not
/// validate, so the endpoint has to.
#[sqlx::test]
async fn an_idempotency_key_beyond_the_contract_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "k".repeat(101),
            "payment_method_id": method,
            "amount": "10.00",
            "allocations": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        problem["detail"],
        "the idempotency key exceeds 100 characters"
    );
    assert_eq!(account_payment_count(&pool, customer).await, 0);
}

/// The endpoint publishes a 422 for a bad amount, so it owes the sentence: the
/// ledger's own guards run only after the movement exists.
#[sqlx::test]
async fn a_non_positive_or_sub_cent_amount_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // The precedence is part of what the boundary says: sign first, then
    // precision, then magnitude. Each sentence is pinned, not just the status,
    // and the last row breaks two rules at once — a negative sub-cent amount —
    // to prove the order is real: the sign sentence wins because the amount
    // never reaches the domain where precision is measured.
    for (amount, detail) in [
        ("0.00", "the amount must be positive"),
        ("-5.00", "the amount must be positive"),
        ("10.005", "the amount 10.005 carries more than two decimals"),
        ("-0.005", "the amount must be positive"),
    ] {
        let (status, problem) = send(
            &router,
            "POST",
            &format!("/api/v1/customers/{customer}/receipts"),
            Some(&cookie),
            Some(json!({
                "payment_method_id": method,
                "amount": amount,
                "allocations": [],
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{amount} was accepted as a collection"
        );
        assert_eq!(problem["detail"], detail, "wrong sentence for {amount}");
    }

    assert_eq!(account_payment_count(&pool, customer).await, 0);
}

/// Only blank keys are discarded. A non-blank key keeps its exact bytes, so a
/// padded key is its own identity: trimming it would make two distinct
/// collections collapse onto one receipt, and a receipt recorded under a padded
/// key before this rule existed would stop matching its own retry.
#[sqlx::test]
async fn a_padded_key_keeps_its_own_identity(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let sale = seed_completed_sale(&pool, customer, dec!(100.00)).await;
    charge_sale(&pool, customer, sale, dec!(100.00)).await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let collect = |key: &'static str| {
        let body = json!({
            "idempotency_key": key,
            "payment_method_id": method,
            "amount": "10.00",
            "allocations": [],
        });
        let router = router.clone();
        let cookie = cookie.clone();
        async move {
            send(
                &router,
                "POST",
                &format!("/api/v1/customers/{customer}/receipts"),
                Some(&cookie),
                Some(body),
            )
            .await
        }
    };

    let (first, _) = collect("  padded  ").await;
    assert_eq!(first, StatusCode::CREATED);

    // The same bytes is a replay; the trimmed form is a different key and a
    // different collection.
    let (same, _) = collect("  padded  ").await;
    assert_eq!(same, StatusCode::OK, "a padded key stopped matching itself");

    let (trimmed, _) = collect("padded").await;
    assert_eq!(
        trimmed,
        StatusCode::CREATED,
        "a key that differs only in padding was collapsed onto the first"
    );

    assert_eq!(account_payment_count(&pool, customer).await, 2);
}

/// The cap measures the characters the client sent, so a key of exactly the
/// limit is accepted. Nothing pinned the accept side before.
#[sqlx::test]
async fn a_key_of_exactly_the_cap_is_accepted(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "k".repeat(100),
            "payment_method_id": method,
            "amount": "10.00",
            "allocations": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(account_payment_count(&pool, customer).await, 1);
}

/// The cap measures the key exactly as sent, padding included: a key whose
/// raw length is over the limit is refused even when its trimmed length
/// would fit, because measuring after trimming is the same rewrite of the
/// key namespace the padded-identity rule exists to prevent.
#[sqlx::test]
async fn a_key_that_only_fits_after_trimming_is_rejected(pool: PgPool) {
    let customer = seed_customer(&pool, "Ana").await;
    let (method, _wallet) = seed_method(&pool, "Cash").await;

    let router = app(pool.clone());
    let (_owner, owner_password) = seed_owner(&pool).await;
    let cookie = login(&router, "owner@pyme.com", &owner_password).await;

    // 101 characters as sent, two of them the padding; 99 once trimmed.
    let key = format!(" {} ", "k".repeat(99));
    assert_eq!(key.chars().count(), 101);

    let (status, problem) = send(
        &router,
        "POST",
        &format!("/api/v1/customers/{customer}/receipts"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": key,
            "payment_method_id": method,
            "amount": "10.00",
            "allocations": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        problem["detail"],
        "the idempotency key exceeds 100 characters"
    );
    assert_eq!(account_payment_count(&pool, customer).await, 0);
}
