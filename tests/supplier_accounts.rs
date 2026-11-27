//! Supplier accounts HTTP integration tests (S1, S2, S3, S5, S7).
//!
//! Covers every HTTP route of the payables ledger:
//! - `GET /api/v1/suppliers/{id}/account`
//! - `POST /api/v1/suppliers/{id}/payments`
//! - `GET /api/v1/suppliers/{id}/payments`
//! - `POST /api/v1/suppliers/{id}/credit-refunds`
//! - `POST /api/v1/suppliers/{id}/adjustments`
//! - `GET /api/v1/suppliers/{id}/adjustments`
//! - `GET /api/v1/accounts/payables`

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self as auth_repo, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::ProductKind;
use organox::catalog::service::{self as catalog, CreateProductInput};
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

/// Test-only helper to backdate `completed_at` for aging bucket tests.
/// Temporarily disables `purchases_prevent_closed_mutation` inside a single
/// transaction so production keeps strict immutability without loopholes.
async fn backdate_completed_purchase(pool: &PgPool, purchase_id: Uuid, days_ago: i32) {
    let mut tx = pool.begin().await.expect("begin backdate tx");
    sqlx::query("ALTER TABLE purchases DISABLE TRIGGER purchases_prevent_closed_mutation")
        .execute(&mut *tx)
        .await
        .expect("disable immutability trigger");
    sqlx::query(
        "UPDATE purchases SET completed_at = NOW() - ($1::int * INTERVAL '1 day') WHERE id = $2",
    )
    .bind(days_ago)
    .bind(purchase_id)
    .execute(&mut *tx)
    .await
    .expect("backdate purchase");
    sqlx::query("ALTER TABLE purchases ENABLE TRIGGER purchases_prevent_closed_mutation")
        .execute(&mut *tx)
        .await
        .expect("re-enable immutability trigger");
    tx.commit().await.expect("commit backdate tx");
}

async fn seed_owner(pool: &PgPool) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse("owner@pyme.com").expect("email");
    let hash = password::hash(&password).expect("hash");
    let id = auth_repo::create_user(
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
) -> (StatusCode, String, Value) {
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
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, content_type, parsed)
}

async fn seed_method(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
    let wallet_id = wallets::create_wallet(pool, &format!("{name} Wallet"))
        .await
        .expect("wallet");
    let method_id = wallets::create_method(pool, wallets::NewMethod { name, wallet_id })
        .await
        .expect("method");
    (wallet_id, method_id)
}

async fn wallet_balance(pool: &PgPool, wallet_id: Uuid) -> Decimal {
    wallets::get_wallet(pool, wallet_id)
        .await
        .expect("wallet query")
        .balance
}

async fn create_supplier(
    router: &axum::Router,
    cookie: &str,
    name: &str,
    tax_id: Option<&str>,
    credit_limit: Option<&str>,
) -> Uuid {
    let (status, _, body) = send(
        router,
        "POST",
        "/api/v1/suppliers",
        Some(cookie),
        Some(json!({
            "name": name,
            "tax_id": tax_id,
            "credit_limit": credit_limit,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create supplier: {body}");
    Uuid::parse_str(body["id"].as_str().expect("id")).expect("uuid")
}

async fn seed_good(pool: &PgPool, name: &str) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(dec!(100.00)),
            min_stock: None,
        },
    )
    .await
    .expect("product")
}

async fn complete_on_account_purchase(
    router: &axum::Router,
    cookie: &str,
    supplier_id: Uuid,
    product_id: Uuid,
    quantity: &str,
    unit_cost: &str,
    on_account: &str,
) -> (Uuid, Value) {
    let (status, _, draft) = send(
        router,
        "POST",
        "/api/v1/purchases/draft",
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "open draft: {draft}");
    let purchase_id = Uuid::parse_str(draft["id"].as_str().expect("purchase id")).expect("uuid");

    let (status, _, body) = send(
        router,
        "PUT",
        &format!("/api/v1/purchases/{purchase_id}/supplier"),
        Some(cookie),
        Some(json!({ "supplier_id": supplier_id })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "attach supplier: {body}");

    let (status, _, body) = send(
        router,
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/items"),
        Some(cookie),
        Some(json!({
            "product_id": product_id,
            "quantity": quantity,
            "unit_cost": unit_cost,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add item: {body}");

    let (status, _, completed) = send(
        router,
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/complete"),
        Some(cookie),
        Some(json!({ "payments": [], "on_account": on_account })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete purchase: {completed}");
    (purchase_id, completed)
}

async fn supplier_entry_count(pool: &PgPool, supplier_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM supplier_account_entries WHERE supplier_id = $1")
        .bind(supplier_id)
        .fetch_one(pool)
        .await
        .expect("entry count")
}

async fn supplier_wallet_movement_count(pool: &PgPool, supplier_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM wallet_movements WHERE supplier_id = $1")
        .bind(supplier_id)
        .fetch_one(pool)
        .await
        .expect("movement count")
}

// =========================================================================
// 1. Authentication: unauthenticated 401 on every supplier_accounts route
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn unauthenticated_requests_return_401_on_all_supplier_account_routes(pool: PgPool) {
    let app = app(pool);
    let id = Uuid::now_v7();
    let method_id = Uuid::now_v7();

    let routes = [
        ("GET", format!("/api/v1/suppliers/{id}/account"), None),
        (
            "POST",
            format!("/api/v1/suppliers/{id}/payments"),
            Some(json!({
                "payment_method_id": method_id,
                "amount": "10.00",
                "allocations": [],
            })),
        ),
        ("GET", format!("/api/v1/suppliers/{id}/payments"), None),
        (
            "POST",
            format!("/api/v1/suppliers/{id}/credit-refunds"),
            Some(json!({
                "payment_method_id": method_id,
                "amount": "10.00",
                "note": "refund",
            })),
        ),
        (
            "POST",
            format!("/api/v1/suppliers/{id}/adjustments"),
            Some(json!({
                "direction": "in",
                "amount": "10.00",
                "note": "opening",
            })),
        ),
        ("GET", format!("/api/v1/suppliers/{id}/adjustments"), None),
        ("GET", "/api/v1/accounts/payables".to_string(), None),
    ];

    for (method, uri, body) in routes {
        let (status, content_type, parsed) = send(&app, method, &uri, None, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must return 401: {parsed}"
        );
        assert_eq!(content_type, "application/problem+json");
        assert_eq!(parsed["status"], 401);
    }
}

// =========================================================================
// 2. GET /suppliers/{id}/account — statement, running balance, open purchases
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn account_statement_lists_entries_chronologically_with_running_balance_and_open_purchases(
    pool: PgPool,
) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool, "Bank").await;
    let product_id = seed_good(&pool, "Flour 25kg").await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Mill Co", Some("30-11111111-1"), None).await;

    // Unknown supplier returns 404 problem+json.
    let (status, ct, problem) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{}/account", Uuid::now_v7()),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown supplier: {problem}");
    assert_eq!(ct, "application/problem+json");

    // Empty account before any entry.
    let (status, _, empty) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty account: {empty}");
    assert_eq!(empty["supplier_id"], supplier_id.to_string());
    assert_eq!(empty["balance"], "0.00");
    assert_eq!(empty["unapplied_credit"], "0.00");
    assert_eq!(empty["outstanding_total"], "0.00");
    assert_eq!(empty["entries"].as_array().unwrap().len(), 0);
    assert_eq!(empty["outstanding_purchases"].as_array().unwrap().len(), 0);

    // 1. Opening adjustment +50.00 -> balance = 50.00
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "50.00", "note": "Opening balance" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // 2. Purchase 1 on account +200.00 -> balance = 250.00
    let (p1_id, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "2",
        "100.00",
        "200.00",
    )
    .await;

    // 3. Purchase 2 on account +120.00 -> balance = 370.00
    let (p2_id, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "2",
        "60.00",
        "120.00",
    )
    .await;

    // 4. Payment -230.00: allocate 200.00 to p1 (closes p1) and 20.00 to p2; 10.00 unapplied
    //    balance = 140.00, unapplied_credit = 10.00, outstanding_total = 100.00 (p2 only)
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "230.00",
            "allocations": [
                { "purchase_id": p1_id, "amount": "200.00" },
                { "purchase_id": p2_id, "amount": "20.00" },
            ],
            "note": "Wire transfer #1",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (status, _, stmt) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "statement: {stmt}");
    assert_eq!(stmt["supplier_id"], supplier_id.to_string());
    assert_eq!(stmt["balance"], "140.00");
    assert_eq!(stmt["unapplied_credit"], "10.00");
    assert_eq!(stmt["outstanding_total"], "100.00");

    let entries = stmt["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0]["kind"], "adjustment_in");
    assert_eq!(entries[0]["amount"], "50.00");
    assert_eq!(entries[0]["running_balance"], "50.00");
    assert_eq!(entries[0]["note"], "Opening balance");

    assert_eq!(entries[1]["kind"], "purchase_charge");
    assert_eq!(entries[1]["purchase_id"], p1_id.to_string());
    assert_eq!(entries[1]["amount"], "200.00");
    assert_eq!(entries[1]["running_balance"], "250.00");

    assert_eq!(entries[2]["kind"], "purchase_charge");
    assert_eq!(entries[2]["purchase_id"], p2_id.to_string());
    assert_eq!(entries[2]["amount"], "120.00");
    assert_eq!(entries[2]["running_balance"], "370.00");

    assert_eq!(entries[3]["kind"], "payment");
    assert_eq!(entries[3]["amount"], "-230.00");
    assert_eq!(entries[3]["running_balance"], "140.00");
    assert!(entries[3]["wallet_movement_id"].is_string());

    let open = stmt["outstanding_purchases"].as_array().expect("open");
    assert_eq!(open.len(), 1, "p1 is fully paid, only p2 remains open");
    assert_eq!(open[0]["purchase_id"], p2_id.to_string());
    assert!(open[0]["completed_at"].is_string());
    assert_eq!(open[0]["charged"], "120.00");
    assert_eq!(open[0]["allocated"], "20.00");
    assert_eq!(open[0]["outstanding"], "100.00");
}

#[sqlx::test(migrations = "./migrations")]
async fn supplier_statement_windows_entries_and_keeps_the_running_balance(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let supplier_id = create_supplier(&app, &cookie, "Window Mill", None, None).await;

    for (amount, note) in [("10.00", "one"), ("20.00", "two"), ("30.00", "three")] {
        let (status, _, body) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": note })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, _, full) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full statement: {full}");
    assert_eq!(full["entries"].as_array().unwrap().len(), 3);
    assert_eq!(full["entries_total"], 3);

    // The window keeps the running balance of the whole ledger: the entry at
    // offset 1 still reports 30.00, not 20.00 counted from the page start.
    let (status, _, page) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account?limit=2&offset=1"),
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
    let (status, _, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account?limit=2&offset=9"),
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
async fn supplier_statement_service_clamps_limit_and_offset(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let supplier_id = create_supplier(&app, &cookie, "Clamp Mill", None, None).await;

    let empty = organox::supplier_accounts::service::account_statement(&pool, supplier_id, 10, 0)
        .await
        .expect("empty statement");
    assert!(empty.entries.is_empty());
    assert_eq!(empty.entries_total, 0);

    for (amount, note) in [("10.00", "one"), ("20.00", "two"), ("30.00", "three")] {
        let (status, _, body) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": note })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let clamped = organox::supplier_accounts::service::account_statement(&pool, supplier_id, 0, -5)
        .await
        .expect("service clamps limit and negative offset");
    assert_eq!(clamped.entries.len(), 1);
    assert_eq!(clamped.entries_total, 3);
    assert_eq!(clamped.entries[0].running_balance, dec!(10.00));
}

#[sqlx::test(migrations = "./migrations")]
async fn supplier_statement_defaults_to_100_entries_per_page(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;
    let supplier_id = create_supplier(&app, &cookie, "High Volume Mill", None, None).await;

    for i in 0..101 {
        sqlx::query(
            "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, note, created_by) \
             VALUES ($1, $2, 'adjustment_in', 1.00, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(supplier_id)
        .bind(format!("entry-{i}"))
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Omitting limit returns the default 100 entries, never an unbounded ledger.
    let (status, _, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "default page: {page1}");
    let entries1 = page1["entries"].as_array().unwrap();
    assert_eq!(entries1.len(), 100);
    assert_eq!(page1["entries_total"], 101);
    assert_eq!(entries1[99]["running_balance"], "100.00");

    let (status, _, page2) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/account?offset=100"),
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
// 3. POST /suppliers/{id}/payments — allocations, advance, idempotency, guards
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn supplier_payments_allocated_unallocated_idempotency_and_wallet_outflow(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (wallet_id, method_id) = seed_method(&pool, "Bank Transfer").await;
    let product_id = seed_good(&pool, "Sugar 50kg").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Sweet Supply", None, None).await;
    let (purchase_id, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "3",
        "100.00",
        "300.00",
    )
    .await;

    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(0.00));

    // Allocated payment with idempotency key: fresh write -> 201, wallet decreases by 180.00.
    let key_100 = "k".repeat(100);
    let (status, _, body) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": key_100,
            "payment_method_id": method_id,
            "amount": "180.00",
            "allocations": [{ "purchase_id": purchase_id, "amount": "150.00" }],
            "note": "  Partial payment + advance  ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh payment: {body}");
    assert_eq!(body["balance"], "120.00");
    assert_eq!(body["unapplied_credit"], "30.00");
    assert_eq!(body["allocations"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["allocations"][0]["purchase_id"],
        purchase_id.to_string()
    );
    assert_eq!(body["allocations"][0]["outstanding"], "150.00");
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-180.00));

    let payment_id = body["payment_id"].as_str().unwrap().to_string();
    let movement_id = body["wallet_movement_id"].as_str().unwrap().to_string();

    // Replay with the same idempotency key -> 200, same IDs, wallet untouched.
    let (status, _, replay) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": key_100,
            "payment_method_id": method_id,
            "amount": "180.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay payment: {replay}");
    assert_eq!(replay["payment_id"], payment_id);
    assert_eq!(replay["wallet_movement_id"], movement_id);
    assert_eq!(replay["balance"], "120.00");
    assert_eq!(replay["unapplied_credit"], "30.00");
    assert_eq!(
        replay["allocations"][0]["purchase_id"],
        purchase_id.to_string()
    );
    assert_eq!(replay["allocations"][0]["outstanding"], "150.00");
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-180.00));

    // Unallocated advance with blank idempotency key -> normalized to None, writes fresh 201 twice.
    for expected_wallet in [dec!(-200.00), dec!(-220.00)] {
        let (status, _, adv) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/payments"),
            Some(&cookie),
            Some(json!({
                "idempotency_key": "   ",
                "payment_method_id": method_id,
                "amount": "20.00",
                "allocations": [],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "blank key advance: {adv}");
        assert_eq!(adv["allocations"].as_array().unwrap().len(), 0);
        assert_eq!(wallet_balance(&pool, wallet_id).await, expected_wallet);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn supplier_payments_reject_invalid_inputs_without_touching_wallet_or_ledger(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (wallet_id, method_id) = seed_method(&pool, "Cash").await;
    let (inactive_wallet_id, method_on_inactive_wallet) = seed_method(&pool, "Old Bank").await;
    let (_active_wallet2, inactive_method_id) = seed_method(&pool, "Disabled Method").await;
    let product_id = seed_good(&pool, "Salt").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    sqlx::query("UPDATE wallets SET is_active = false WHERE id = $1")
        .bind(inactive_wallet_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE payment_methods SET is_active = false WHERE id = $1")
        .bind(inactive_method_id)
        .execute(&pool)
        .await
        .unwrap();

    let supplier_a = create_supplier(&app, &cookie, "Supplier A", None, None).await;
    let supplier_b = create_supplier(&app, &cookie, "Supplier B", None, None).await;

    let (purchase_a, _) = complete_on_account_purchase(
        &app, &cookie, supplier_a, product_id, "1", "100.00", "100.00",
    )
    .await;
    let (purchase_b, _) = complete_on_account_purchase(
        &app, &cookie, supplier_b, product_id, "1", "100.00", "100.00",
    )
    .await;

    // Draft purchase (not completed) attached to supplier_a.
    let (_, _, draft) = send(&app, "POST", "/api/v1/purchases/draft", Some(&cookie), None).await;
    let draft_id = Uuid::parse_str(draft["id"].as_str().unwrap()).unwrap();
    let (st, _, _) = send(
        &app,
        "PUT",
        &format!("/api/v1/purchases/{draft_id}/supplier"),
        Some(&cookie),
        Some(json!({ "supplier_id": supplier_a })),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let entries_before = supplier_entry_count(&pool, supplier_a).await;
    let movements_before = supplier_wallet_movement_count(&pool, supplier_a).await;
    let balance_before = wallet_balance(&pool, wallet_id).await;

    let overlong_key = "x".repeat(101);
    let cases = vec![
        (
            "overlong idempotency key",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "idempotency_key": overlong_key,
                "payment_method_id": method_id,
                "amount": "10.00",
                "allocations": [],
            }),
        ),
        (
            "duplicate purchase in allocations",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "50.00",
                "allocations": [
                    { "purchase_id": purchase_a, "amount": "20.00" },
                    { "purchase_id": purchase_a, "amount": "20.00" },
                ],
            }),
        ),
        (
            "over-allocation beyond purchase outstanding",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "150.00",
                "allocations": [{ "purchase_id": purchase_a, "amount": "100.01" }],
            }),
        ),
        (
            "allocations sum exceeds payment amount",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "40.00",
                "allocations": [{ "purchase_id": purchase_a, "amount": "50.00" }],
            }),
        ),
        (
            "wrong supplier purchase",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "50.00",
                "allocations": [{ "purchase_id": purchase_b, "amount": "50.00" }],
            }),
        ),
        (
            "draft purchase",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "50.00",
                "allocations": [{ "purchase_id": draft_id, "amount": "50.00" }],
            }),
        ),
        (
            "unknown purchase",
            StatusCode::NOT_FOUND,
            json!({
                "payment_method_id": method_id,
                "amount": "50.00",
                "allocations": [{ "purchase_id": Uuid::now_v7(), "amount": "50.00" }],
            }),
        ),
        (
            "inactive payment method",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": inactive_method_id,
                "amount": "10.00",
                "allocations": [],
            }),
        ),
        (
            "inactive wallet",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_on_inactive_wallet,
                "amount": "10.00",
                "allocations": [],
            }),
        ),
        (
            "unknown payment method",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": Uuid::now_v7(),
                "amount": "10.00",
                "allocations": [],
            }),
        ),
        (
            "zero amount",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "0.00",
                "allocations": [],
            }),
        ),
        (
            "too many decimals",
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "payment_method_id": method_id,
                "amount": "10.005",
                "allocations": [],
            }),
        ),
    ];

    for (label, expected_status, payload) in cases {
        let (status, ct, problem) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_a}/payments"),
            Some(&cookie),
            Some(payload),
        )
        .await;
        assert_eq!(status, expected_status, "{label}: {problem}");
        assert_eq!(ct, "application/problem+json", "{label}");
    }

    // Unknown supplier returns 404.
    let (status, ct, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{}/payments", Uuid::now_v7()),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "10.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(ct, "application/problem+json");

    // Nothing was written on any rejection.
    assert_eq!(
        supplier_entry_count(&pool, supplier_a).await,
        entries_before
    );
    assert_eq!(
        supplier_wallet_movement_count(&pool, supplier_a).await,
        movements_before
    );
    assert_eq!(wallet_balance(&pool, wallet_id).await, balance_before);
}

// =========================================================================
// 4. GET /suppliers/{id}/payments?limit= — log, allocations, clamping
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn payment_log_returns_newest_first_with_allocations_and_clamps_limit(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (wallet_id, method_id) = seed_method(&pool, "Wire").await;
    let product_id = seed_good(&pool, "Yeast").await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Yeast Labs", None, None).await;
    let (p1, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "1",
        "100.00",
        "100.00",
    )
    .await;

    // Unknown supplier -> 404.
    let (status, _, _) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{}/payments", Uuid::now_v7()),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Record 3 payments.
    let mut ids = Vec::new();
    for (idx, alloc) in [
        json!([{ "purchase_id": p1, "amount": "30.00" }]),
        json!([]),
        json!([{ "purchase_id": p1, "amount": "20.00" }]),
    ]
    .into_iter()
    .enumerate()
    {
        let (st, _, res) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/payments"),
            Some(&cookie),
            Some(json!({
                "payment_method_id": method_id,
                "amount": "40.00",
                "allocations": alloc,
                "note": format!("Payment #{}", idx + 1),
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        ids.push(res["payment_id"].as_str().unwrap().to_string());
    }

    // Default list: newest first.
    let (st, _, list) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["payment_id"], ids[2]);
    assert_eq!(items[0]["wallet_id"], wallet_id.to_string());
    assert_eq!(items[0]["payment_method_id"], method_id.to_string());
    assert_eq!(items[0]["amount"], "40.00");
    assert_eq!(items[0]["allocated_total"], "20.00");
    assert_eq!(items[0]["allocations"][0]["purchase_id"], p1.to_string());
    assert_eq!(items[0]["allocations"][0]["amount"], "20.00");
    assert_eq!(items[0]["note"], "Payment #3");

    assert_eq!(items[1]["payment_id"], ids[1]);
    assert_eq!(items[1]["allocated_total"], "0.00");
    assert_eq!(items[1]["allocations"].as_array().unwrap().len(), 0);

    assert_eq!(items[2]["payment_id"], ids[0]);
    assert_eq!(items[2]["allocated_total"], "30.00");

    // limit=0 clamps to 1.
    let (st, _, clamped_min) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments?limit=0"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(clamped_min.as_array().unwrap().len(), 1);
    assert_eq!(clamped_min[0]["payment_id"], ids[2]);

    // limit=999 clamps to 200 and succeeds.
    let (st, _, clamped_max) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments?limit=999"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(clamped_max.as_array().unwrap().len(), 3);

    // offset pages through newest-first rows; negative offset clamps to 0.
    let (st, _, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments?limit=1&offset=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let page1_items = page1.as_array().unwrap();
    assert_eq!(page1_items.len(), 1);
    assert_eq!(page1_items[0]["payment_id"], ids[1]);

    let (st, _, clamped_offset) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments?limit=1&offset=-2"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(clamped_offset.as_array().unwrap()[0]["payment_id"], ids[2]);

    // Past the end the log ends the way a paging client expects: 200 and an
    // empty array, never an error or the last page again.
    let (st, _, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/payments?limit=1&offset=50"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(beyond.as_array().unwrap().is_empty());
}

// =========================================================================
// 5. POST /suppliers/{id}/credit-refunds — cap, inflow, idempotency, guards
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn credit_refunds_enforce_cap_increase_wallet_and_support_idempotency(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (wallet_id, method_id) = seed_method(&pool, "Treasury").await;
    let product_id = seed_good(&pool, "Boxes").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Box Maker", None, None).await;

    // Create an 80.00 open purchase + 100.00 unallocated advance:
    // balance = -20.00, unapplied_credit = 100.00 -> refundable cap = min(100, 20) = 20.00.
    let (_, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "1",
        "80.00",
        "80.00",
    )
    .await;
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "shared-key",
            "payment_method_id": method_id,
            "amount": "100.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-100.00));

    // Blank note -> 422.
    let (st, ct, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "10.00",
            "note": "   ",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(ct, "application/problem+json");

    // Exceeding min(unapplied_credit, -balance) = 20.00 -> 422.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "20.01",
            "note": "Too much",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-100.00));

    // Reusing an idempotency key from a payment -> 409 Conflict.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "shared-key",
            "payment_method_id": method_id,
            "amount": "10.00",
            "note": "Key collision",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-100.00));

    // Valid refund of 20.00 -> 201, wallet INCREASES from -100.00 to -80.00.
    let (st, _, refund) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "refund-1",
            "payment_method_id": method_id,
            "amount": "20.00",
            "note": "Supplier returned excess advance",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "refund: {refund}");
    assert_eq!(refund["balance"], "0.00");
    assert_eq!(refund["unapplied_credit"], "80.00");
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-80.00));

    // Replay -> 200 with same IDs, wallet unchanged.
    let (st, _, replay) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "idempotency_key": "refund-1",
            "payment_method_id": method_id,
            "amount": "20.00",
            "note": "Supplier returned excess advance",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "replay refund: {replay}");
    assert_eq!(replay["refund_id"], refund["refund_id"]);
    assert_eq!(replay["wallet_movement_id"], refund["wallet_movement_id"]);
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-80.00));

    // Now balance is 0.00 (even though unapplied_credit is 80.00) -> further refund rejected (422).
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "1.00",
            "note": "Nothing refundable",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(wallet_balance(&pool, wallet_id).await, dec!(-80.00));
}

// =========================================================================
// 6. POST & GET /suppliers/{id}/adjustments — guards, no wallet movement, log
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn adjustments_enforce_note_and_non_negative_balance_without_wallet_movement(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool, "Cash").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Paper Mill", None, None).await;

    // Blank note -> 422.
    let (st, ct, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "50.00", "note": "  " })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(ct, "application/problem+json");

    // adjustment_out on zero balance -> 422.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "1.00", "note": "Cross zero" })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // adjustment_in +100.00 -> 201, no wallet movement.
    let (st, _, adj1) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "100.00", "note": "Opening balance" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "adj_in: {adj1}");
    assert_eq!(adj1["kind"], "adjustment_in");
    assert_eq!(adj1["balance"], "100.00");
    assert_eq!(adj1["unapplied_credit"], "0.00");
    assert_eq!(supplier_wallet_movement_count(&pool, supplier_id).await, 0);

    // adjustment_out exceeding balance (100.01) -> 422.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "100.01", "note": "Too large" })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // adjustment_out -100.00 down to exact 0.00 -> 201.
    let (st, _, adj2) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "100.00", "note": "Written off" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "adj_out: {adj2}");
    assert_eq!(adj2["kind"], "adjustment_out");
    assert_eq!(adj2["balance"], "0.00");
    assert_eq!(supplier_wallet_movement_count(&pool, supplier_id).await, 0);

    // While holding advance credit (balance < 0), adjustment_out -> 422.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "25.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "out", "amount": "5.00", "note": "While holding credit" })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // GET adjustments log: newest first, supports limit clamping.
    let (st, _, log) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let items = log.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["adjustment_id"], adj2["adjustment_id"]);
    assert_eq!(items[0]["kind"], "adjustment_out");
    assert_eq!(items[0]["amount"], "-100.00");
    assert_eq!(items[0]["note"], "Written off");
    assert_eq!(items[1]["adjustment_id"], adj1["adjustment_id"]);
    assert_eq!(items[1]["kind"], "adjustment_in");
    assert_eq!(items[1]["amount"], "100.00");

    let (st, _, clamped) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments?limit=0"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(clamped.as_array().unwrap().len(), 1);
    assert_eq!(clamped[0]["adjustment_id"], adj2["adjustment_id"]);

    let (st, _, page1) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments?limit=1&offset=1"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let page1_items = page1.as_array().unwrap();
    assert_eq!(page1_items.len(), 1);
    assert_eq!(page1_items[0]["adjustment_id"], adj1["adjustment_id"]);

    // A negative offset is the first page, not a database error.
    let (st, _, clamped_offset) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments?limit=1&offset=-5"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        clamped_offset.as_array().unwrap()[0]["adjustment_id"],
        adj2["adjustment_id"]
    );

    // Past the end the log ends the way a paging client expects: 200 and an
    // empty array, never an error or the last page again.
    let (st, _, beyond) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments?limit=1&offset=50"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(beyond.as_array().unwrap().is_empty());

    // Unknown supplier -> 404.
    let (st, _, _) = send(
        &app,
        "GET",
        &format!("/api/v1/suppliers/{}/adjustments", Uuid::now_v7()),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn payables_windows_items_and_keeps_the_portfolio_totals(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    for (name, amount) in [("First Mill", "10.00"), ("Second Mill", "20.00")] {
        let supplier_id = create_supplier(&app, &cookie, name, None, None).await;
        let (status, _, body) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": amount, "note": "opening" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, _, full) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full report: {full}");
    assert_eq!(full["items"].as_array().unwrap().len(), 2);
    assert_eq!(full["items_total"], 2);

    // One page of items, totals over the whole report.
    let (status, _, page) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?limit=1",
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
    let (status, _, beyond) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?limit=1&offset=5",
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
async fn payables_handles_empty_window_and_portfolio_totals_above_single_column_cap(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool);
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let (status, _, empty) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?limit=10",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty report: {empty}");
    assert_eq!(empty["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty["items_total"], 0);
    assert_eq!(empty["total_balance"], "0.00");

    for name in ["Whale Mill One", "Whale Mill Two"] {
        let supplier_id = create_supplier(&app, &cookie, name, None, None).await;
        let (status, _, body) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": "in", "amount": "600000000000.00", "note": "opening" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "adjustment: {body}");
    }

    let (status, _, page) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?limit=1",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "large portfolio page: {page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items_total"], 2);
    assert_eq!(page["total_balance"], "1200000000000.00");
    assert_eq!(page["total_adjustments"], "1200000000000.00");

    let (status, _, beyond) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?limit=1&offset=5",
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
async fn payables_defaults_to_50_items_per_page(pool: PgPool) {
    let (owner_id, pw) = seed_owner(&pool).await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    for i in 0..51 {
        let supplier_id = Uuid::now_v7();
        sqlx::query("INSERT INTO suppliers (id, name) VALUES ($1, $2)")
            .bind(supplier_id)
            .bind(format!("Creditor {i:02}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, note, created_by) \
             VALUES ($1, $2, 'adjustment_in', 10.00, 'opening', $3)",
        )
        .bind(Uuid::now_v7())
        .bind(supplier_id)
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Omitting limit returns the default 50 items while totals span all 51.
    let (status, _, page1) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "default payables: {page1}");
    assert_eq!(page1["items"].as_array().unwrap().len(), 50);
    assert_eq!(page1["items_total"], 51);
    assert_eq!(page1["total_balance"], "510.00");

    let (status, _, page2) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables?offset=50",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second payables page: {page2}");
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["items_total"], 51);
    assert_eq!(page2["total_balance"], "510.00");
}

// =========================================================================
// 7. GET /accounts/payables — totals, 4 aging buckets, sorting, filtering
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn payables_report_computes_totals_aging_buckets_and_sorts_open_positions(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool, "Bank").await;
    let product_id = seed_good(&pool, "Grain").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    // Supplier 1: settled (balance = 0, unapplied_credit = 0) -> MUST be excluded.
    let settled_id = create_supplier(&app, &cookie, "Settled Co", None, None).await;
    let (settled_purchase, _) =
        complete_on_account_purchase(&app, &cookie, settled_id, product_id, "1", "50.00", "50.00")
            .await;
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{settled_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "50.00",
            "allocations": [{ "purchase_id": settled_purchase, "amount": "50.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Supplier 2: open purchases across all 4 aging buckets at exact boundaries:
    // 0d (10) + 30d (20) = 30.00
    // 31d (40) + 60d (50) = 90.00
    // 61d (60) + 90d (70) = 130.00
    // 91d (80) = 80.00
    // outstanding_total = 330.00, adjustment_in = 25.00 -> balance = 355.00.
    let aging_id =
        create_supplier(&app, &cookie, "Aging Vendor", Some("30-99999999-9"), None).await;
    for (unit_cost, days_ago) in [
        ("10.00", 0_i32),
        ("20.00", 30),
        ("40.00", 31),
        ("50.00", 60),
        ("60.00", 61),
        ("70.00", 90),
        ("80.00", 91),
    ] {
        let (purchase_id, _) = complete_on_account_purchase(
            &app, &cookie, aging_id, product_id, "1", unit_cost, unit_cost,
        )
        .await;
        if days_ago > 0 {
            backdate_completed_purchase(&pool, purchase_id, days_ago).await;
        }
    }
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{aging_id}/adjustments"),
        Some(&cookie),
        Some(json!({ "direction": "in", "amount": "25.00", "note": "Legacy payable" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Supplier 3: balance == 0 BUT unapplied_credit == 100.00 != 0 -> MUST be included.
    let offset_id = create_supplier(&app, &cookie, "Offsetting Vendor", None, None).await;
    let (offset_purchase, _) = complete_on_account_purchase(
        &app, &cookie, offset_id, product_id, "1", "100.00", "100.00",
    )
    .await;
    backdate_completed_purchase(&pool, offset_purchase, 45).await;
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{offset_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "100.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Supplier 4: advance credit only (balance = -45.00, unapplied_credit = 45.00).
    let advance_id = create_supplier(&app, &cookie, "Advance Vendor", None, None).await;
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{advance_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "45.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (status, _, body) = send(
        &app,
        "GET",
        "/api/v1/accounts/payables",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "payables: {body}");

    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "settled supplier must be excluded: {body}");

    // Sorted by balance DESC: 355.00, 0.00, -45.00.
    assert_eq!(items[0]["supplier_id"], aging_id.to_string());
    assert_eq!(items[0]["supplier_name"], "Aging Vendor");
    assert_eq!(items[0]["tax_id"], "30-99999999-9");
    assert_eq!(items[0]["is_active"], true);
    assert_eq!(items[0]["balance"], "355.00");
    assert_eq!(items[0]["unapplied_credit"], "0.00");
    assert_eq!(items[0]["outstanding_total"], "330.00");
    assert_eq!(items[0]["adjustments"], "25.00");
    assert_eq!(items[0]["aging"]["days_0_30"], "30.00");
    assert_eq!(items[0]["aging"]["days_31_60"], "90.00");
    assert_eq!(items[0]["aging"]["days_61_90"], "130.00");
    assert_eq!(items[0]["aging"]["days_90_plus"], "80.00");

    assert_eq!(items[1]["supplier_id"], offset_id.to_string());
    assert_eq!(items[1]["balance"], "0.00");
    assert_eq!(items[1]["unapplied_credit"], "100.00");
    assert_eq!(items[1]["outstanding_total"], "100.00");
    assert_eq!(items[1]["aging"]["days_31_60"], "100.00");

    assert_eq!(items[2]["supplier_id"], advance_id.to_string());
    assert_eq!(items[2]["balance"], "-45.00");
    assert_eq!(items[2]["unapplied_credit"], "45.00");
    assert_eq!(items[2]["outstanding_total"], "0.00");

    // Totals match sum across items.
    assert_eq!(body["total_balance"], "310.00");
    assert_eq!(body["total_unapplied_credit"], "145.00");
    assert_eq!(body["total_outstanding"], "430.00");
    assert_eq!(body["total_adjustments"], "25.00");
    assert_eq!(body["aging"]["days_0_30"], "30.00");
    assert_eq!(body["aging"]["days_31_60"], "190.00");
    assert_eq!(body["aging"]["days_61_90"], "130.00");
    assert_eq!(body["aging"]["days_90_plus"], "80.00");
}

#[sqlx::test(migrations = "./migrations")]
async fn supplier_payment_allocation_rejects_decimal_max_without_panicking(pool: PgPool) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool, "Bank").await;
    let product_id = seed_good(&pool, "Grain").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Grain Co", None, None).await;
    let (purchase_id, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "2",
        "50.00",
        "100.00",
    )
    .await;

    // Allocation exceeding numeric(14,2) on POST /payments -> 422 and no wallet movement.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "50.00",
            "allocations": [{ "purchase_id": purchase_id, "amount": Decimal::MAX.to_string() }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(supplier_wallet_movement_count(&pool, supplier_id).await, 0);

    // Partial allocation followed by direct allocate(Decimal::MAX) must return AmountTooLarge, not panic.
    let (st, _, created) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "50.00",
            "allocations": [{ "purchase_id": purchase_id, "amount": "10.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let entry_id: Uuid = created["payment_id"].as_str().unwrap().parse().unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = organox::supplier_accounts::repository::allocate(
        &mut tx,
        entry_id,
        purchase_id,
        Decimal::MAX,
    )
    .await
    .expect_err("Decimal::MAX allocation must fail");
    assert!(matches!(
        err,
        organox::supplier_accounts::repository::RepoError::AmountTooLarge { .. }
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn adjustment_in_reconciles_over_adjusted_supplier_account_and_preserves_advance_credit(
    pool: PgPool,
) {
    let (_owner_id, pw) = seed_owner(&pool).await;
    let (_wallet_id, method_id) = seed_method(&pool, "Bank").await;
    let product_id = seed_good(&pool, "Flour").await;
    let app = app(pool.clone());
    let cookie = login(&app, "owner@pyme.com", &pw).await;

    let supplier_id = create_supplier(&app, &cookie, "Mill Co", None, None).await;
    let (purchase_id, _) = complete_on_account_purchase(
        &app,
        &cookie,
        supplier_id,
        product_id,
        "2",
        "50.00",
        "100.00",
    )
    .await;

    // 1. adjustment_out $30 -> balance = 70, outstanding = 100.
    let (st, _, adj_out) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "out",
            "amount": "30.00",
            "note": "manual payable correction",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "adj_out: {adj_out}");
    assert_eq!(adj_out["balance"], "70.00");

    // 2. Pay the full $100 purchase -> balance = -30, U = 0, outstanding = 0.
    let (st, _, pay) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "100.00",
            "allocations": [{ "purchase_id": purchase_id, "amount": "100.00" }],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "payment: {pay}");
    assert_eq!(pay["balance"], "-30.00");
    assert_eq!(pay["unapplied_credit"], "0.00");

    // 3. Add $20 unallocated advance -> balance = -50, U = 20, non_credit_drift = 30.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/payments"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "20.00",
            "allocations": [],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // adjustment_out and adjustment_in > 30.00 are rejected with 422.
    for (direction, amount) in [("out", "1.00"), ("in", "30.01")] {
        let (st, _, _) = send(
            &app,
            "POST",
            &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
            Some(&cookie),
            Some(json!({ "direction": direction, "amount": amount, "note": "must fail" })),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Reconciling the exact 30.00 non-credit drift succeeds and preserves 20.00 advance credit.
    let (st, _, adj_in) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "30.00",
            "note": "reconcile over-adjustment",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "reconcile: {adj_in}");
    assert_eq!(adj_in["balance"], "-20.00");
    assert_eq!(adj_in["unapplied_credit"], "20.00");

    // Once only real advance credit remains (drift = 0), adjustment_in is refused again.
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/adjustments"),
        Some(&cookie),
        Some(json!({
            "direction": "in",
            "amount": "0.01",
            "note": "cannot bury advance credit",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // Refund the 20.00 advance credit -> balance = 0 -> deactivate succeeds (204).
    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/credit-refunds"),
        Some(&cookie),
        Some(json!({
            "payment_method_id": method_id,
            "amount": "20.00",
            "note": "supplier refunded advance",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _, _) = send(
        &app,
        "POST",
        &format!("/api/v1/suppliers/{supplier_id}/deactivate"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}
