//! Customer account read surfaces over HTTP (S13):
//!
//! - `GET /api/v1/customers/{id}/account`: statement entries oldest first with
//!   running balance, current balance, `unapplied_credit`, `outstanding_total`,
//!   and every sale with `outstanding > 0`.
//! - `GET /api/v1/customers/{id}/receipts`: S6 collection write log.
//! - `GET /api/v1/customers/{id}/adjustments`: S11 adjustment write log.
//! - `GET /api/v1/accounts/receivables`: customers with `balance != 0` or
//!   `unapplied_credit != 0`, with aging buckets computed from each open sale's
//!   `completed_at` date (`0-30`, `31-60`, `61-90`, `90+`).
//! - `GET /api/v1/customers` and `GET /api/v1/customers/{id}` expose `balance`,
//!   `available_credit`, and `credit_limit`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
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

/// Test-only helper to age a completed sale for receivables aging-bucket tests.
/// Temporarily disables the closed-sale immutability trigger inside a single
/// transaction so production keeps strict immutability without loopholes.
async fn backdate_completed_sale(pool: &PgPool, sale_id: Uuid, days_ago: i32) {
    let mut tx = pool.begin().await.expect("begin backdate tx");
    sqlx::query("ALTER TABLE sales DISABLE TRIGGER sales_prevent_closed_mutation")
        .execute(&mut *tx)
        .await
        .expect("disable immutability trigger");
    sqlx::query(
        "UPDATE sales SET completed_at = NOW() - ($1::int * INTERVAL '1 day') WHERE id = $2",
    )
    .bind(days_ago)
    .bind(sale_id)
    .execute(&mut *tx)
    .await
    .expect("backdate sale");
    sqlx::query("ALTER TABLE sales ENABLE TRIGGER sales_prevent_closed_mutation")
        .execute(&mut *tx)
        .await
        .expect("re-enable immutability trigger");
    tx.commit().await.expect("commit backdate tx");
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
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

async fn seed_method(pool: &PgPool) -> (Uuid, Uuid) {
    let wallet_id = wallets::create_wallet(pool, "Cash Register")
        .await
        .expect("wallet");
    let method_id = wallets::create_method(
        pool,
        wallets::NewMethod {
            name: "Cash",
            wallet_id,
        },
    )
    .await
    .expect("method");
    (wallet_id, method_id)
}

async fn create_customer(
    router: &axum::Router,
    cookie: &str,
    name: &str,
    tax_id: Option<&str>,
    credit_limit: Option<&str>,
) -> Uuid {
    let (status, body) = send(
        router,
        "POST",
        "/api/v1/customers",
        Some(cookie),
        Some(json!({
            "name": name,
            "tax_id": tax_id,
            "credit_limit": credit_limit,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create customer: {body}");
    Uuid::parse_str(body["id"].as_str().expect("id")).expect("uuid")
}

async fn create_product_with_stock(
    router: &axum::Router,
    cookie: &str,
    sku: &str,
    price: &str,
    stock: &str,
) -> Uuid {
    let (status, body) = send(
        router,
        "POST",
        "/api/v1/products",
        Some(cookie),
        Some(json!({
            "sku": sku,
            "name": format!("Item {sku}"),
            "kind": "product",
            "price": price,
            "min_stock": "0.00",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create product: {body}");
    let id = Uuid::parse_str(body["id"].as_str().expect("id")).expect("uuid");

    let (status, body) = send(
        router,
        "POST",
        &format!("/api/v1/products/{id}/movements"),
        Some(cookie),
        Some(json!({
            "reason": "purchase",
            "quantity": stock,
            "unit_cost": "10.00",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed stock: {body}");
    id
}

async fn complete_on_account_sale(
    router: &axum::Router,
    cookie: &str,
    customer_id: Uuid,
    product_id: Uuid,
    quantity: &str,
    on_account: &str,
    cash_payment: Option<(Uuid, &str)>,
) -> (Uuid, Value) {
    let (status, draft) = send(router, "POST", "/api/v1/sales/draft", Some(cookie), None).await;
    assert_eq!(status, StatusCode::CREATED, "open draft: {draft}");
    let sale_id = Uuid::parse_str(draft["id"].as_str().expect("sale id")).expect("uuid");

    let (status, body) = send(
        router,
        "PUT",
        &format!("/api/v1/sales/{sale_id}/customer"),
        Some(cookie),
        Some(json!({ "customer_id": customer_id })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "attach customer: {body}");

    let (status, body) = send(
        router,
        "POST",
        &format!("/api/v1/sales/{sale_id}/items"),
        Some(cookie),
        Some(json!({ "product_id": product_id, "quantity": quantity })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add item: {body}");

    let payments = match cash_payment {
        Some((method_id, amount)) => json!([{ "payment_method_id": method_id, "amount": amount }]),
        None => json!([]),
    };

    let (status, completed) = send(
        router,
        "POST",
        &format!("/api/v1/sales/{sale_id}/complete"),
        Some(cookie),
        Some(json!({ "payments": payments, "on_account": on_account })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete sale: {completed}");
    (sale_id, completed)
}

fn parse_dec(v: &Value) -> Decimal {
    v.as_str()
        .expect("decimal string")
        .parse()
        .expect("valid decimal")
}

// =========================================================================
// 1. GET /customers/{id}/account — statement, running balance, open sales
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn account_statement_lists_entries_oldest_first_with_running_balance_and_open_sales(
    pool: PgPool,
) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let customer_id = create_customer(&app, &cookie, "Acme Co", Some("20-11111111-1"), None).await;
    let product_id = create_product_with_stock(&app, &cookie, "SKU-1", "100.00", "20.00").await;

    // Empty account before any entry.
    let (status, empty) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty account: {empty}");
    assert_eq!(empty["customer_id"], customer_id.to_string());
    assert_eq!(empty["balance"], "0.00");
    assert_eq!(empty["unapplied_credit"], "0.00");
    assert_eq!(empty["outstanding_total"], "0.00");
    assert_eq!(empty["entries"].as_array().unwrap().len(), 0);
    assert_eq!(empty["outstanding_sales"].as_array().unwrap().len(), 0);

    // 1. Opening adjustment +50.00 -> balance = 50.00
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "50.00", "note": "Opening balance" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 2. Sale 1 on account +200.00 -> balance = 250.00
    let (sale1_id, _) = complete_on_account_sale(
        &app,
        &cookie,
        customer_id,
        product_id,
        "2.00",
        "200.00",
        None,
    )
    .await;

    // 3. Receipt 180.00: allocate 120.00 to sale 1, leave 60.00 as unapplied credit -> balance = 70.00
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "180.00",
            "allocations": [{ "sale_id": sale1_id, "amount": "120.00" }],
            "note": "Partial payment plus advance",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 4. Sale 2 on account +100.00 -> auto-applies 60.00 credit, owes 40.00 -> balance = 170.00
    let (sale2_id, completed2) = complete_on_account_sale(
        &app,
        &cookie,
        customer_id,
        product_id,
        "1.00",
        "100.00",
        None,
    )
    .await;
    assert_eq!(completed2["credit_applied"], "60.00");

    // 5. Credit note on sale 1 for 1 unit (100.00): cancels 80.00 remaining debt on sale 1,
    //    refunds 20.00 cash -> balance = 90.00, sale 1 fully settled (outstanding 0).
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/sales/{sale1_id}/credit-notes"),
        Some(&cookie),
        Some(json!({
            "reason": "Damaged unit",
            "items": [{ "product_id": product_id, "quantity": "1.00" }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 6. Adjustment out -20.00 -> balance = 70.00 (adj net = +30.00, sale2 outstanding = 40.00, U = 0)
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "20.00", "note": "Rounding correction" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Read full statement
    let (status, stmt) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "statement: {stmt}");
    assert_eq!(stmt["balance"], "70.00");
    assert_eq!(stmt["unapplied_credit"], "0.00");
    assert_eq!(stmt["outstanding_total"], "40.00");

    let entries = stmt["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 6);

    let expected = [
        ("adjustment_in", "50.00", "50.00"),
        ("sale_charge", "200.00", "250.00"),
        ("receipt", "-180.00", "70.00"),
        ("sale_charge", "100.00", "170.00"),
        ("credit_note", "-80.00", "90.00"),
        ("adjustment_out", "-20.00", "70.00"),
    ];
    for (idx, (kind, amount, running)) in expected.iter().enumerate() {
        assert_eq!(entries[idx]["kind"], *kind, "entry {idx} kind");
        assert_eq!(entries[idx]["amount"], *amount, "entry {idx} amount");
        assert_eq!(
            entries[idx]["running_balance"], *running,
            "entry {idx} running_balance"
        );
    }

    // Only sale 2 still has debt (sale 1 was paid 120 + cancelled 80 = 200 -> 0).
    let open = stmt["outstanding_sales"]
        .as_array()
        .expect("outstanding_sales");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["sale_id"], sale2_id.to_string());
    assert_eq!(open[0]["charged"], "100.00");
    assert_eq!(open[0]["cancelled"], "0.00");
    assert_eq!(open[0]["allocated"], "60.00");
    assert_eq!(open[0]["outstanding"], "40.00");
}

#[sqlx::test(migrations = "./migrations")]
async fn account_read_routes_reject_unknown_customer_and_missing_session(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let missing = Uuid::now_v7();

    for path in [
        format!("/api/v1/customers/{missing}/account"),
        format!("/api/v1/customers/{missing}/receipts"),
        format!("/api/v1/customers/{missing}/adjustments"),
    ] {
        let (status, _) = send(&app, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "unauth {path}");

        let (status, _) = send(&app, "GET", &path, Some(&cookie), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "missing customer {path}");
    }

    let (status, _) = send(&app, "GET", "/api/v1/accounts/receivables", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn account_statement_windows_entries_and_keeps_the_running_balance(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let customer_id = create_customer(&app, &cookie, "Window Co", None, None).await;

    for (amount, note) in [("10.00", "one"), ("20.00", "two"), ("30.00", "three")] {
        let (status, body) = send(
            &app,
            "POST",
            &format!("/api/v1/customers/{customer_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": note })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, full) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full statement: {full}");
    assert_eq!(full["entries"].as_array().unwrap().len(), 3);
    assert_eq!(full["entries_total"], 3);

    // The window keeps the running balance of the whole ledger: the entry at
    // offset 1 still reports 30.00, not 20.00 counted from the page start.
    let (status, page) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account?limit=2&offset=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "windowed statement: {page}");
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(page["entries_total"], 3);
    assert_eq!(entries[0]["running_balance"], "30.00");
    assert_eq!(entries[1]["running_balance"], "60.00");
    // The position is the whole account, never the page.
    assert_eq!(page["balance"], "60.00");
    assert_eq!(page["balance"], full["balance"]);

    // Past the end: no entries, the real total, and the same position.
    let (status, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account?limit=2&offset=9"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "past the end: {beyond}");
    assert_eq!(beyond["entries"].as_array().unwrap().len(), 0);
    assert_eq!(beyond["entries_total"], 3);
    assert_eq!(beyond["balance"], "60.00");
}

#[sqlx::test(migrations = "./migrations")]
async fn account_statement_service_clamps_limit_and_offset(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let customer_id = create_customer(&app, &cookie, "Clamp Co", None, None).await;

    // Empty account at offset 0 returns 0 entries without failing.
    let empty = organox::customer_accounts::service::account_statement(&pool, customer_id, 10, 0)
        .await
        .expect("empty statement");
    assert!(empty.entries.is_empty());
    assert_eq!(empty.entries_total, 0);

    for (amount, note) in [("10.00", "one"), ("20.00", "two"), ("30.00", "three")] {
        let (status, body) = send(
            &app,
            "POST",
            &format!("/api/v1/customers/{customer_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": note })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    // Service must clamp limit to 1..=500 and negative offset to 0 even when
    // called directly without going through the HTTP handler.
    let clamped = organox::customer_accounts::service::account_statement(&pool, customer_id, 0, -5)
        .await
        .expect("service clamps limit and negative offset");
    assert_eq!(clamped.entries.len(), 1);
    assert_eq!(clamped.entries_total, 3);
    assert_eq!(clamped.entries[0].running_balance, dec!(10.00));
}

#[sqlx::test(migrations = "./migrations")]
async fn account_statement_defaults_to_100_entries_per_page(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let customer_id = create_customer(&app, &cookie, "High Volume Co", None, None).await;

    for i in 0..101 {
        sqlx::query(
            "INSERT INTO customer_account_entries (id, customer_id, kind, amount, note, created_by) \
             VALUES ($1, $2, 'adjustment_in', 1.00, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(customer_id)
        .bind(format!("entry-{i}"))
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Omitting limit returns the default 100 entries, never an unbounded ledger.
    let (status, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "default page: {page1}");
    let entries1 = page1["entries"].as_array().unwrap();
    assert_eq!(entries1.len(), 100);
    assert_eq!(page1["entries_total"], 101);
    assert_eq!(entries1[99]["running_balance"], "100.00");

    let (status, page2) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/account?offset=100"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second page: {page2}");
    let entries2 = page2["entries"].as_array().unwrap();
    assert_eq!(entries2.len(), 1);
    assert_eq!(page2["entries_total"], 101);
    assert_eq!(entries2[0]["running_balance"], "101.00");
}

// =========================================================================
// 2. GET /customers/{id}/receipts & GET /customers/{id}/adjustments
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn receipts_log_returns_newest_first_with_allocations_wallet_and_note(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let (wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let customer_id = create_customer(&app, &cookie, "Beta LLC", None, None).await;
    let product_id = create_product_with_stock(&app, &cookie, "SKU-R", "100.00", "10.00").await;
    let (sale_id, _) = complete_on_account_sale(
        &app,
        &cookie,
        customer_id,
        product_id,
        "2.00",
        "200.00",
        None,
    )
    .await;

    let (s1, r1) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "80.00",
            "allocations": [{ "sale_id": sale_id, "amount": "50.00" }],
            "note": "First payment",
        })),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, r2) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "40.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    // Also write an adjustment to prove it does not leak into receipts.
    let (sa, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "10.00", "note": "Not a receipt" })),
    )
    .await;
    assert_eq!(sa, StatusCode::CREATED);

    let (status, list) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/receipts"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "receipts: {list}");
    let items = list.as_array().expect("receipts array");
    assert_eq!(items.len(), 2);

    // Newest first: r2 then r1
    assert_eq!(items[0]["receipt_id"], r2["receipt_id"]);
    assert_eq!(items[0]["wallet_movement_id"], r2["wallet_movement_id"]);
    assert_eq!(items[0]["wallet_id"], wallet_id.to_string());
    assert_eq!(items[0]["payment_method_id"], method_id.to_string());
    assert_eq!(items[0]["amount"], "40.00");
    assert_eq!(items[0]["allocated_total"], "0.00");
    assert_eq!(items[0]["allocations"].as_array().unwrap().len(), 0);
    assert!(items[0]["note"].is_null());
    assert_eq!(items[0]["created_by"], owner_id.to_string());

    assert_eq!(items[1]["receipt_id"], r1["receipt_id"]);
    assert_eq!(items[1]["amount"], "80.00");
    assert_eq!(items[1]["allocated_total"], "50.00");
    assert_eq!(items[1]["note"], "First payment");
    let allocs = items[1]["allocations"].as_array().unwrap();
    assert_eq!(allocs.len(), 1);
    assert_eq!(allocs[0]["sale_id"], sale_id.to_string());
    assert_eq!(allocs[0]["amount"], "50.00");

    // Limit and offset parameters are respected.
    let (status, limited) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/receipts?limit=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let limited_items = limited.as_array().unwrap();
    assert_eq!(limited_items.len(), 1);
    assert_eq!(limited_items[0]["receipt_id"], r2["receipt_id"]);

    let (status, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/receipts?limit=1&offset=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page1_items = page1.as_array().unwrap();
    assert_eq!(page1_items.len(), 1);
    assert_eq!(page1_items[0]["receipt_id"], r1["receipt_id"]);

    let (status, clamped_offset) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/receipts?limit=1&offset=-3"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        clamped_offset.as_array().unwrap()[0]["receipt_id"],
        r2["receipt_id"]
    );

    // Past the end: 200 and an empty array, which is what a paging client
    // loops until.
    let (status, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/receipts?limit=1&offset=50"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(beyond.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn adjustments_log_returns_newest_first_with_signed_amount_and_note(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let customer_id = create_customer(&app, &cookie, "Gamma SA", None, None).await;

    let (s1, a1) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "120.00", "note": "Opening balance" })),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, a2) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "35.50", "note": "Discount error" })),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    // Write a receipt to verify it does not appear in adjustments.
    let (sr, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{customer_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "10.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(sr, StatusCode::CREATED);

    let (status, list) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/adjustments"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "adjustments: {list}");
    let items = list.as_array().expect("adjustments array");
    assert_eq!(items.len(), 2);

    assert_eq!(items[0]["adjustment_id"], a2["adjustment_id"]);
    assert_eq!(items[0]["kind"], "adjustment_out");
    assert_eq!(items[0]["amount"], "-35.50");
    assert_eq!(items[0]["note"], "Discount error");
    assert_eq!(items[0]["created_by"], owner_id.to_string());

    assert_eq!(items[1]["adjustment_id"], a1["adjustment_id"]);
    assert_eq!(items[1]["kind"], "adjustment_in");
    assert_eq!(items[1]["amount"], "120.00");
    assert_eq!(items[1]["note"], "Opening balance");

    let (status, limited) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/adjustments?limit=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(limited.as_array().unwrap().len(), 1);
    assert_eq!(limited[0]["adjustment_id"], a2["adjustment_id"]);

    let (status, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/adjustments?limit=1&offset=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page1_items = page1.as_array().unwrap();
    assert_eq!(page1_items.len(), 1);
    assert_eq!(page1_items[0]["adjustment_id"], a1["adjustment_id"]);

    // A negative offset is the first page, not a database error.
    let (status, clamped) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/adjustments?limit=1&offset=-5"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(clamped.as_array().unwrap().len(), 1);
    assert_eq!(clamped[0]["adjustment_id"], a2["adjustment_id"]);

    // Past the end the log ends the way a paging client expects: 200 and an
    // empty array, never an error or the last page again.
    let (status, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{customer_id}/adjustments?limit=1&offset=50"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(beyond.as_array().unwrap().is_empty());
}

// =========================================================================
// 3. GET /accounts/receivables — filter (balance != 0 OR U != 0) & aging
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn receivables_filters_open_positions_and_buckets_aging_from_sale_date(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let product_id = create_product_with_stock(&app, &cookie, "SKU-A", "10.00", "200.00").await;

    // Customer 1: settled (balance = 0, U = 0) -> MUST be excluded.
    let settled_id = create_customer(&app, &cookie, "Settled Customer", None, None).await;
    let (settled_sale, _) =
        complete_on_account_sale(&app, &cookie, settled_id, product_id, "5.00", "50.00", None)
            .await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{settled_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "50.00",
            "allocations": [{ "sale_id": settled_sale, "amount": "50.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Customer 2: debtor with sales across all 4 aging buckets at exact boundaries:
    // - 0 days:   10.00 (0-30)
    // - 30 days:  20.00 (0-30)  -> days_0_30 = 30.00
    // - 31 days:  40.00 (31-60)
    // - 60 days:  50.00 (31-60) -> days_31_60 = 90.00
    // - 61 days:  60.00 (61-90)
    // - 90 days:  70.00 (61-90) -> days_61_90 = 130.00
    // - 91 days:  80.00 (90+)   -> days_90_plus = 80.00
    // Total outstanding = 330.00, plus opening adjustment +25.00 -> balance = 355.00.
    let aging_id =
        create_customer(&app, &cookie, "Aging Debtor", Some("30-11111111-9"), None).await;
    let bucket_cases = [
        ("1.00", "10.00", 0_i32),
        ("2.00", "20.00", 30),
        ("4.00", "40.00", 31),
        ("5.00", "50.00", 60),
        ("6.00", "60.00", 61),
        ("7.00", "70.00", 90),
        ("8.00", "80.00", 91),
    ];
    for (qty, total, days_ago) in bucket_cases {
        let (sale_id, _) =
            complete_on_account_sale(&app, &cookie, aging_id, product_id, qty, total, None).await;
        if days_ago > 0 {
            backdate_completed_sale(&pool, sale_id, days_ago).await;
        }
    }
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{aging_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "25.00", "note": "Legacy balance" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Customer 3: balance == 0 BUT unapplied_credit == 100.00 != 0 -> MUST be included!
    let zero_bal_u_id = create_customer(&app, &cookie, "Offsetting Position", None, None).await;
    let (offset_sale, _) = complete_on_account_sale(
        &app,
        &cookie,
        zero_bal_u_id,
        product_id,
        "10.00",
        "100.00",
        None,
    )
    .await;
    backdate_completed_sale(&pool, offset_sale, 45).await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{zero_bal_u_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "100.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Customer 4: pure unapplied credit (balance = -45.00, U = 45.00) -> MUST be included!
    let credit_only_id = create_customer(&app, &cookie, "Advance Credit", None, None).await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{credit_only_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "45.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Query receivables
    let (status, body) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "receivables: {body}");

    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3, "settled customer must be excluded: {body}");

    // Ordered by balance DESC: Aging Debtor (355.00), Offsetting Position (0.00), Advance Credit (-45.00)
    assert_eq!(items[0]["customer_id"], aging_id.to_string());
    assert_eq!(items[0]["customer_name"], "Aging Debtor");
    assert_eq!(items[0]["tax_id"], "30-11111111-9");
    assert_eq!(items[0]["is_active"], true);
    assert_eq!(items[0]["balance"], "355.00");
    assert_eq!(items[0]["unapplied_credit"], "0.00");
    assert_eq!(items[0]["outstanding_total"], "330.00");
    assert_eq!(items[0]["adjustments"], "25.00");
    assert_eq!(items[0]["aging"]["days_0_30"], "30.00");
    assert_eq!(items[0]["aging"]["days_31_60"], "90.00");
    assert_eq!(items[0]["aging"]["days_61_90"], "130.00");
    assert_eq!(items[0]["aging"]["days_90_plus"], "80.00");

    assert_eq!(items[1]["customer_id"], zero_bal_u_id.to_string());
    assert_eq!(items[1]["balance"], "0.00");
    assert_eq!(items[1]["unapplied_credit"], "100.00");
    assert_eq!(items[1]["outstanding_total"], "100.00");
    assert_eq!(items[1]["adjustments"], "0.00");
    assert_eq!(items[1]["aging"]["days_0_30"], "0.00");
    assert_eq!(items[1]["aging"]["days_31_60"], "100.00");
    assert_eq!(items[1]["aging"]["days_61_90"], "0.00");
    assert_eq!(items[1]["aging"]["days_90_plus"], "0.00");

    assert_eq!(items[2]["customer_id"], credit_only_id.to_string());
    assert_eq!(items[2]["balance"], "-45.00");
    assert_eq!(items[2]["unapplied_credit"], "45.00");
    assert_eq!(items[2]["outstanding_total"], "0.00");
    assert_eq!(items[2]["adjustments"], "0.00");
    assert_eq!(items[2]["aging"]["days_0_30"], "0.00");
    assert_eq!(items[2]["aging"]["days_31_60"], "0.00");
    assert_eq!(items[2]["aging"]["days_61_90"], "0.00");
    assert_eq!(items[2]["aging"]["days_90_plus"], "0.00");

    // Portfolio totals match sum across items.
    assert_eq!(body["total_balance"], "310.00");
    assert_eq!(body["total_unapplied_credit"], "145.00");
    assert_eq!(body["total_outstanding"], "430.00");
    assert_eq!(body["total_adjustments"], "25.00");
    assert_eq!(body["aging"]["days_0_30"], "30.00");
    assert_eq!(body["aging"]["days_31_60"], "190.00");
    assert_eq!(body["aging"]["days_61_90"], "130.00");
    assert_eq!(body["aging"]["days_90_plus"], "80.00");

    // S2 invariant holds at portfolio level too: balance + U == outstanding + adjustments.
    let total_balance = parse_dec(&body["total_balance"]);
    let total_u = parse_dec(&body["total_unapplied_credit"]);
    let total_out = parse_dec(&body["total_outstanding"]);
    let total_adj = parse_dec(&body["total_adjustments"]);
    assert_eq!(total_balance + total_u, total_out + total_adj);
    assert_eq!(
        parse_dec(&body["aging"]["days_0_30"])
            + parse_dec(&body["aging"]["days_31_60"])
            + parse_dec(&body["aging"]["days_61_90"])
            + parse_dec(&body["aging"]["days_90_plus"]),
        total_out
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn receivables_windows_items_and_keeps_the_portfolio_totals(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    for (name, amount) in [("First Debtor", "10.00"), ("Second Debtor", "20.00")] {
        let customer_id = create_customer(&app, &cookie, name, None, None).await;
        let (status, body) = send(
            &app,
            "POST",
            &format!("/api/v1/customers/{customer_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": "opening" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, full) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full report: {full}");
    assert_eq!(full["items"].as_array().unwrap().len(), 2);
    assert_eq!(full["items_total"], 2);

    // One page of items, totals over the whole report.
    let (status, page) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?limit=1",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "windowed report: {page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items_total"], 2);
    assert_eq!(page["total_balance"], full["total_balance"]);
    assert_eq!(
        page["total_unapplied_credit"],
        full["total_unapplied_credit"]
    );
    assert_eq!(page["total_outstanding"], full["total_outstanding"]);
    assert_eq!(page["aging"], full["aging"]);

    // Past the end: no items, and the totals still describe the report.
    let (status, beyond) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?limit=1&offset=5",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "past the end: {beyond}");
    assert_eq!(beyond["items"].as_array().unwrap().len(), 0);
    assert_eq!(beyond["items_total"], 2);
    assert_eq!(beyond["total_balance"], full["total_balance"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn receivables_handles_empty_window_and_portfolio_totals_above_single_column_cap(
    pool: PgPool,
) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    // Empty report with a window at offset 0 returns zeroes cleanly.
    let (status, empty) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?limit=10",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty report: {empty}");
    assert_eq!(empty["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty["items_total"], 0);
    assert_eq!(empty["total_balance"], "0.00");

    // Two customers each within numeric(14, 2) whose combined portfolio balance
    // exceeds numeric(14, 2) (1.2e12 > 9.99e11) must not overflow SQL casts.
    for name in ["Whale One", "Whale Two"] {
        let customer_id = create_customer(&app, &cookie, name, None, None).await;
        let (status, body) = send(
            &app,
            "POST",
            &format!("/api/v1/customers/{customer_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": "600000000000.00", "note": "opening" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, page) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?limit=1",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "large portfolio page: {page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items_total"], 2);
    assert_eq!(page["total_balance"], "1200000000000.00");
    assert_eq!(page["total_adjustments"], "1200000000000.00");

    let (status, beyond) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?limit=1&offset=5",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "large portfolio past end: {beyond}");
    assert_eq!(beyond["items"].as_array().unwrap().len(), 0);
    assert_eq!(beyond["items_total"], 2);
    assert_eq!(beyond["total_balance"], "1200000000000.00");
}

#[sqlx::test(migrations = "./migrations")]
async fn receivables_defaults_to_50_items_per_page(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    for i in 0..51 {
        let customer_id = Uuid::now_v7();
        sqlx::query("INSERT INTO customers (id, name) VALUES ($1, $2)")
            .bind(customer_id)
            .bind(format!("Debtor {i:02}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO customer_account_entries (id, customer_id, kind, amount, note, created_by) \
             VALUES ($1, $2, 'adjustment_in', 10.00, 'opening', $3)",
        )
        .bind(Uuid::now_v7())
        .bind(customer_id)
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Omitting limit returns the default 50 items while totals span all 51.
    let (status, page1) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "default receivables: {page1}");
    assert_eq!(page1["items"].as_array().unwrap().len(), 50);
    assert_eq!(page1["items_total"], 51);
    assert_eq!(page1["total_balance"], "510.00");

    let (status, page2) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables?offset=50",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second receivables page: {page2}");
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["items_total"], 51);
    assert_eq!(page2["total_balance"], "510.00");
}

// =========================================================================
// 4. GET /customers & GET /customers/{id} — balance, available_credit, limit
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn customers_read_endpoints_expose_balance_available_credit_and_credit_limit(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let product_id = create_product_with_stock(&app, &cookie, "SKU-C", "100.00", "10.00").await;

    let alice_id = create_customer(
        &app,
        &cookie,
        "Alice",
        Some("20-10000000-1"),
        Some("500.00"),
    )
    .await;
    let bob_id = create_customer(&app, &cookie, "Bob", Some("20-20000000-2"), None).await;

    // Fresh customer defaults: 0.00 / 0.00 / credit_limit as set.
    let (status, fresh) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{bob_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fresh["balance"], "0.00");
    assert_eq!(fresh["available_credit"], "0.00");
    assert!(fresh["credit_limit"].is_null());

    // Alice: 100 on account, plus 140 receipt (40 allocated to sale, 100 left unapplied):
    // balance = 100 - 140 = -40.00, available_credit = 100.00.
    // Wait: let's also refund 25.00 credit -> balance = -15.00, available_credit = 75.00!
    // Wait: refund requires balance + amount <= 0 -> -40 + 25 = -15 <= 0 (valid!).
    let (sale_id, _) =
        complete_on_account_sale(&app, &cookie, alice_id, product_id, "1.00", "100.00", None).await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{alice_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "140.00",
            "allocations": [{ "sale_id": sale_id, "amount": "40.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{alice_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "25.00",
            "note": "Partial refund",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Check GET /customers/{id}
    let (status, alice) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{alice_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(alice["balance"], "-15.00");
    assert_eq!(alice["available_credit"], "75.00");
    assert_eq!(alice["credit_limit"], "500.00");

    // Check GET /customers list (pagination + search + derived columns)
    let (status, list) = send(
        &app,
        "GET",
        "/api/v1/customers?limit=10&offset=0",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 2);
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "Alice");
    assert_eq!(items[0]["balance"], "-15.00");
    assert_eq!(items[0]["available_credit"], "75.00");
    assert_eq!(items[0]["credit_limit"], "500.00");
    assert_eq!(items[1]["name"], "Bob");
    assert_eq!(items[1]["balance"], "0.00");
    assert_eq!(items[1]["available_credit"], "0.00");
    assert!(items[1]["credit_limit"].is_null());

    // Empty page past offset still reports true total (existing behavior holds).
    let (status, empty_page) = send(
        &app,
        "GET",
        "/api/v1/customers?limit=10&offset=50",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty_page["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty_page["total"], 2);
    assert_eq!(parse_dec(&alice["available_credit"]), dec!(75.00));
}

#[sqlx::test(migrations = "./migrations")]
async fn statement_and_receivables_cover_credit_refunds_partial_aging_empty_and_inactive_history(
    pool: PgPool,
) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    // Empty portfolio returns 200 with zeroed totals and buckets.
    let (status, empty_rec) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty_rec["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty_rec["total_balance"], "0.00");
    assert_eq!(empty_rec["total_unapplied_credit"], "0.00");
    assert_eq!(empty_rec["total_outstanding"], "0.00");
    assert_eq!(empty_rec["total_adjustments"], "0.00");
    assert_eq!(empty_rec["aging"]["days_0_30"], "0.00");
    assert_eq!(empty_rec["aging"]["days_90_plus"], "0.00");

    let product_id = create_product_with_stock(&app, &cookie, "SKU-E", "50.00", "20.00").await;

    // Customer A: receives 100.00 credit, refunds 100.00, then is deactivated ->
    // historical statement still readable with credit_refund entry (+100.00) and balance 0.00.
    let hist_id = create_customer(&app, &cookie, "Historical Co", None, None).await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{hist_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "100.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{hist_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "100.00",
            "note": "Full refund before closing account",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{hist_id}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, hist_stmt) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{hist_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hist_stmt["balance"], "0.00");
    let entries = hist_stmt["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kind"], "receipt");
    assert_eq!(entries[0]["amount"], "-100.00");
    assert_eq!(entries[0]["running_balance"], "-100.00");
    assert_eq!(entries[1]["kind"], "credit_refund");
    assert_eq!(entries[1]["amount"], "100.00");
    assert_eq!(entries[1]["running_balance"], "0.00");

    // limit=0 clamps to 1 rather than returning empty or failing.
    let (st, clamped) = send(
        &app,
        "GET",
        &format!("/api/v1/customers/{hist_id}/receipts?limit=0"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(clamped.as_array().unwrap().len(), 1);

    // Customer B: 200.00 sale completed 75 days ago, 50.00 credited + 30.00 allocated ->
    // aging bucket 61-90 must hold only the 120.00 remaining outstanding, not 200.00.
    let partial_id = create_customer(&app, &cookie, "Partial Aging", None, None).await;
    let (sale_id, _) = complete_on_account_sale(
        &app, &cookie, partial_id, product_id, "4.00", "200.00", None,
    )
    .await;
    backdate_completed_sale(&pool, sale_id, 75).await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/sales/{sale_id}/credit-notes"),
        Some(&cookie),
        Some(json!({
            "reason": "Returned 1 unit",
            "items": [{ "product_id": product_id, "quantity": "1.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = send(
        &app,
        "POST",
        &format!("/api/v1/customers/{partial_id}/receipts"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "30.00",
            "allocations": [{ "sale_id": sale_id, "amount": "30.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, rec) = send(
        &app,
        "GET",
        "/api/v1/accounts/receivables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let items = rec["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["customer_id"], partial_id.to_string());
    assert_eq!(items[0]["balance"], "120.00");
    assert_eq!(items[0]["outstanding_total"], "120.00");
    assert_eq!(items[0]["aging"]["days_61_90"], "120.00");
}
