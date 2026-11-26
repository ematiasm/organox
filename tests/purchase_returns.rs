//! Purchase returns (supplier returns) HTTP & concurrency integration tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self as auth_repo, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self as catalog, CreateProductInput, RecordMovementInput};
use organox::payments::service::{self as payments, CreateMethodInput, CreateWalletInput};
use organox::purchases::model::PurchasePaymentInput;
use organox::purchases::service as purchases;
use organox::routes::router;
use organox::supplier_accounts::repository as supplier_accounts;
use organox::suppliers::service::{self as suppliers, CreateSupplierInput};
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

async fn seed_user(pool: &PgPool, email: &str, role: Role) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(&password).expect("hash");
    let id = auth_repo::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role,
        },
    )
    .await
    .expect("user");
    (id, password)
}

async fn login_cookie(app: &axum::Router, email: &str, password: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn call_json(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let req = match body {
        Some(payload) => builder
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    (status, json)
}

async fn seed_product(pool: &PgPool, name: &str) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(dec!(50.00)),
            min_stock: None,
        },
    )
    .await
    .expect("product")
}

async fn seed_supplier(pool: &PgPool, name: &str) -> Uuid {
    suppliers::create_supplier(
        pool,
        CreateSupplierInput {
            name: name.to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("supplier")
}

async fn seed_wallet_and_method(
    pool: &PgPool,
    wallet_name: &str,
    method_name: &str,
) -> (Uuid, Uuid) {
    let wallet_id = payments::create_wallet(
        pool,
        CreateWalletInput {
            name: wallet_name.to_string(),
        },
    )
    .await
    .expect("wallet");
    let method_id = payments::create_method(
        pool,
        CreateMethodInput {
            name: method_name.to_string(),
            wallet_id,
        },
    )
    .await
    .expect("method");
    (wallet_id, method_id)
}

async fn complete_purchase(
    pool: &PgPool,
    actor: Uuid,
    supplier_id: Uuid,
    items: &[(Uuid, Decimal, Decimal)],
    wallet_payments: &[(Uuid, Decimal)],
    on_account: Decimal,
) -> Uuid {
    let (draft_id, _) = purchases::open_draft(pool, actor).await.expect("draft");
    purchases::set_supplier(pool, draft_id, Some(supplier_id))
        .await
        .expect("set supplier");
    for &(product_id, qty, unit_cost) in items {
        purchases::add_item(pool, draft_id, product_id, qty, unit_cost)
            .await
            .expect("add item");
    }
    let payment_inputs: Vec<PurchasePaymentInput> = wallet_payments
        .iter()
        .map(|&(payment_method_id, amount)| PurchasePaymentInput {
            payment_method_id,
            amount,
        })
        .collect();
    purchases::complete(pool, draft_id, actor, &payment_inputs, on_account, None)
        .await
        .expect("complete purchase");
    draft_id
}

#[sqlx::test]
async fn unauthenticated_requests_return_401(pool: PgPool) {
    let app = app(pool);
    let id = Uuid::now_v7();

    let (status_get, _) = call_json(
        &app,
        None,
        "GET",
        &format!("/api/v1/purchases/{id}/returns"),
        None,
    )
    .await;
    assert_eq!(status_get, StatusCode::UNAUTHORIZED);

    let (status_post, _) = call_json(
        &app,
        None,
        "POST",
        &format!("/api/v1/purchases/{id}/returns"),
        Some(json!({ "reason": "Damaged", "items": [] })),
    )
    .await;
    assert_eq!(status_post, StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn cash_purchase_multi_tender_returns_split_proportionally_and_restore_wallets(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-cash@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-cash@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "MultiTender Supplier").await;
    let product = seed_product(&pool, "Olive Oil").await;
    let (w1, m1) = seed_wallet_and_method(&pool, "Cash Box", "Cash").await;
    let (w2, m2) = seed_wallet_and_method(&pool, "Bank A", "Transfer A").await;
    let (w3, m3) = seed_wallet_and_method(&pool, "Bank B", "Transfer B").await;

    // Total 150.00 split across 3 tenders (15 + 45 + 90 = 150).
    let purchase_id = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(15.00))],
        &[(m1, dec!(15.00)), (m2, dec!(45.00)), (m3, dec!(90.00))],
        dec!(0.00),
    )
    .await;

    // GET summary before any returns.
    let (status, summary) = call_json(
        &app,
        Some(&cookie),
        "GET",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["purchase_total"], "150.00");
    assert_eq!(summary["returned_total"], "0.00");
    assert_eq!(summary["returnable_total"], "150.00");
    assert_eq!(summary["purchase_outstanding"], "0.00");
    assert_eq!(summary["wallet_refundable_capacity"], "150.00");
    assert_eq!(summary["items"][0]["returnable_quantity"], "10");

    // Return #1: 3 units = 45.00 via wallet_refund.
    let (status, ret1) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Broken bottles",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "3" }],
            "idempotency_key": "ret-1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ret1["total"], "45.00");
    assert_eq!(ret1["debt_cancelled"], "0.00");
    assert_eq!(ret1["credit_granted"], "0.00");
    assert_eq!(ret1["surplus_mode"], "wallet_refund");
    assert_eq!(ret1["items"][0]["unit_cost"], "15.00");
    assert_eq!(ret1["items"][0]["subtotal"], "45.00");
    assert_eq!(ret1["refunds"].as_array().unwrap().len(), 3);

    // Stock decreased 10 -> 7 with `purchase_return` movement carrying frozen unit_cost.
    let stock = catalog::get_product(&pool, product).await.unwrap().stock;
    assert_eq!(stock, Some(dec!(7)));
    let moves = catalog::list_movements(&pool, product, 10).await.unwrap();
    assert_eq!(moves[0].reason, MovementReason::PurchaseReturn);
    assert_eq!(moves[0].quantity, dec!(3));
    assert_eq!(moves[0].signed_quantity, dec!(-3));
    assert_eq!(moves[0].unit_cost, Some(dec!(15.00)));

    // Replay with same idempotency key returns 200 and writes nothing new.
    let (status_replay, replay) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Broken bottles",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "3" }],
            "idempotency_key": "ret-1"
        })),
    )
    .await;
    assert_eq!(status_replay, StatusCode::OK);
    assert_eq!(replay["id"], ret1["id"]);
    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(7))
    );

    // Return #2: remaining 7 units = 105.00 -> recovers exact full wallet balances.
    let (status, ret2) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Rest of lot recalled",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "7" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ret2["total"], "105.00");

    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(0))
    );
    assert_eq!(
        payments::get_wallet(&pool, w1).await.unwrap().balance,
        dec!(0.00)
    );
    assert_eq!(
        payments::get_wallet(&pool, w2).await.unwrap().balance,
        dec!(0.00)
    );
    assert_eq!(
        payments::get_wallet(&pool, w3).await.unwrap().balance,
        dec!(0.00)
    );

    // Trying to return 1 more unit is refused with 409.
    let (status_over, err) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Too late",
            "items": [{ "product_id": product, "quantity": "1" }]
        })),
    )
    .await;
    assert_eq!(status_over, StatusCode::CONFLICT);
    assert!(err["detail"].as_str().unwrap().contains("0 left to return"));
}

#[sqlx::test]
async fn supplier_credit_surplus_applies_to_next_purchase_and_blocks_wallet_refund_on_credit_paid_purchase(
    pool: PgPool,
) {
    let (actor, pw) = seed_user(&pool, "ret-credit@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-credit@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Credit Supplier").await;
    let product = seed_product(&pool, "Coffee Beans").await;
    let (wallet, method) = seed_wallet_and_method(&pool, "Main Bank", "Wire").await;

    // Purchase #1: 10 @ 20.00 = 200.00 paid cash.
    let p1 = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(20.00))],
        &[(method, dec!(200.00))],
        dec!(0.00),
    )
    .await;

    // Return 4 units (80.00) as supplier_credit.
    let (status, ret) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{p1}/returns"),
        Some(json!({
            "reason": "Keep as credit for next order",
            "surplus_mode": "supplier_credit",
            "items": [{ "product_id": product, "quantity": "4" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ret["total"], "80.00");
    assert_eq!(ret["debt_cancelled"], "0.00");
    assert_eq!(ret["credit_granted"], "80.00");
    assert_eq!(ret["refunds"].as_array().unwrap().len(), 0);

    // Wallet untouched (-200.00); supplier account has balance -80, unapplied_credit 80.
    assert_eq!(
        payments::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(-200.00)
    );
    let pos = supplier_accounts::account_position(&pool, supplier)
        .await
        .unwrap();
    assert_eq!(pos.balance, dec!(-80.00));
    assert_eq!(pos.unapplied_credit, dec!(80.00));
    assert_eq!(pos.outstanding_total, dec!(0.00));
    assert!(pos.is_consistent());

    // Purchase #2: 4 @ 20.00 = 80.00 fully on account -> auto-covered 100% by the 80.00 return credit!
    let p2 = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(4), dec!(20.00))],
        &[],
        dec!(80.00),
    )
    .await;

    let pos2 = supplier_accounts::account_position(&pool, supplier)
        .await
        .unwrap();
    assert_eq!(pos2.balance, dec!(0.00));
    assert_eq!(pos2.unapplied_credit, dec!(0.00));
    assert_eq!(pos2.outstanding_total, dec!(0.00));
    assert!(pos2.is_consistent());

    // Purchase #2 was settled entirely with return credit (0 wallet money):
    // returning with wallet_refund MUST fail with 409 and write nothing.
    let (status_bad, err_bad) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{p2}/returns"),
        Some(json!({
            "reason": "Try cash refund on credit-paid purchase",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "2" }]
        })),
    )
    .await;
    assert_eq!(status_bad, StatusCode::CONFLICT);
    assert!(
        err_bad["detail"]
            .as_str()
            .unwrap()
            .contains("supplier_credit")
    );
    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(10))
    );

    // Returning p2 with supplier_credit succeeds and restores 40.00 of supplier credit.
    let (status_ok, ret2) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{p2}/returns"),
        Some(json!({
            "reason": "Return back to credit",
            "surplus_mode": "supplier_credit",
            "items": [{ "product_id": product, "quantity": "2" }]
        })),
    )
    .await;
    assert_eq!(status_ok, StatusCode::CREATED);
    assert_eq!(ret2["credit_granted"], "40.00");

    // Supplier can then refund that 40.00 credit in cash via existing credit-refunds endpoint!
    let (status_cf, cf) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/suppliers/{supplier}/credit-refunds"),
        Some(json!({
            "payment_method_id": method,
            "amount": "40.00",
            "note": "Supplier wired back return credit"
        })),
    )
    .await;
    assert_eq!(status_cf, StatusCode::CREATED);
    assert_eq!(cf["balance"], "0.00");
    assert_eq!(cf["unapplied_credit"], "0.00");
    assert_eq!(
        payments::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(-160.00)
    );
}

#[sqlx::test]
async fn mixed_on_account_purchase_cancels_debt_first_then_settles_surplus_by_mode(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-mixed@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-mixed@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Mixed Supplier").await;
    let product = seed_product(&pool, "Sugar").await;
    let (wallet, method) = seed_wallet_and_method(&pool, "Till", "Cash").await;

    // Purchase A: 10 @ 10.00 = 100.00 -> 40.00 cash + 60.00 on account.
    let pa = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(10.00))],
        &[(method, dec!(40.00))],
        dec!(60.00),
    )
    .await;

    // Return 8 units (80.00) with wallet_refund -> 60.00 cancels debt, 20.00 refunded to wallet.
    let (status, ret_a) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{pa}/returns"),
        Some(json!({
            "reason": "Torn bags",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "8" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ret_a["total"], "80.00");
    assert_eq!(ret_a["debt_cancelled"], "60.00");
    assert_eq!(ret_a["credit_granted"], "0.00");
    assert_eq!(ret_a["refunds"][0]["amount"], "20.00");
    assert_eq!(
        payments::get_wallet(&pool, wallet).await.unwrap().balance,
        dec!(-20.00)
    );

    let (st_acc, acc) = call_json(
        &app,
        Some(&cookie),
        "GET",
        &format!("/api/v1/suppliers/{supplier}/account"),
        None,
    )
    .await;
    assert_eq!(st_acc, StatusCode::OK);
    assert_eq!(acc["balance"], "0.00");
    assert_eq!(acc["outstanding_total"], "0.00");
    assert_eq!(acc["entries"].as_array().unwrap().len(), 2);
    assert_eq!(acc["entries"][1]["kind"], "purchase_return");
    assert_eq!(acc["entries"][1]["purchase_return_id"], ret_a["id"]);

    // Purchase B: 10 @ 10.00 = 100.00 -> 40.00 cash + 60.00 on account.
    let pb = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(10.00))],
        &[(method, dec!(40.00))],
        dec!(60.00),
    )
    .await;

    // Return 8 units (80.00) with supplier_credit -> 60.00 cancels debt, 20.00 supplier credit.
    let (status, ret_b) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{pb}/returns"),
        Some(json!({
            "reason": "Credit remainder",
            "surplus_mode": "supplier_credit",
            "items": [{ "product_id": product, "quantity": "8" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ret_b["debt_cancelled"], "60.00");
    assert_eq!(ret_b["credit_granted"], "20.00");
    assert_eq!(ret_b["refunds"].as_array().unwrap().len(), 0);

    let pos = supplier_accounts::account_position(&pool, supplier)
        .await
        .unwrap();
    assert_eq!(pos.balance, dec!(-20.00));
    assert_eq!(pos.unapplied_credit, dec!(20.00));
    assert_eq!(pos.outstanding_total, dec!(0.00));
    assert!(pos.is_consistent());
}

#[sqlx::test]
async fn stock_check_zero_cost_lines_inactive_supplier_and_validation_guards_hold(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-guards@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-guards@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Guard Supplier").await;
    let p_regular = seed_product(&pool, "Flour").await;
    let p_promo = seed_product(&pool, "Promo Sample").await;
    let p_other = seed_product(&pool, "Not On Purchase").await;
    let (_wallet, method) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    // Draft purchase cannot be returned (409).
    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();
    let (st_draft, _) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{draft_id}/returns"),
        Some(json!({
            "reason": "Draft return",
            "items": [{ "product_id": p_regular, "quantity": "1" }]
        })),
    )
    .await;
    assert_eq!(st_draft, StatusCode::CONFLICT);
    purchases::cancel(&pool, draft_id).await.unwrap();

    // Unknown purchase -> 404.
    let (st_404, _) = call_json(
        &app,
        Some(&cookie),
        "GET",
        &format!("/api/v1/purchases/{}/returns", Uuid::now_v7()),
        None,
    )
    .await;
    assert_eq!(st_404, StatusCode::NOT_FOUND);

    // Completed purchase with regular (10 @ 10.00) + promo zero-cost (5 @ 0.00).
    let purchase_id = complete_purchase(
        &pool,
        actor,
        supplier,
        &[
            (p_regular, dec!(10), dec!(10.00)),
            (p_promo, dec!(5), dec!(0.00)),
        ],
        &[(method, dec!(100.00))],
        dec!(0.00),
    )
    .await;

    // Validation 422 cases: empty items, blank reason, duplicate product, unknown product, bad precision.
    for payload in [
        json!({ "reason": "Empty", "items": [] }),
        json!({ "reason": "   ", "items": [{ "product_id": p_regular, "quantity": "1" }] }),
        json!({ "reason": "Dup", "items": [
            { "product_id": p_regular, "quantity": "1" },
            { "product_id": p_regular, "quantity": "1" }
        ] }),
        json!({ "reason": "Wrong product", "items": [{ "product_id": p_other, "quantity": "1" }] }),
        json!({ "reason": "Too precise", "items": [{ "product_id": p_regular, "quantity": "1.1234" }] }),
        json!({ "reason": "Blank key", "idempotency_key": "   ", "items": [{ "product_id": p_regular, "quantity": "1" }] }),
    ] {
        let (st, _) = call_json(
            &app,
            Some(&cookie),
            "POST",
            &format!("/api/v1/purchases/{purchase_id}/returns"),
            Some(payload),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Consume 7 units of p_regular (3 left in stock). Returning 4 must fail 409 (insufficient stock).
    catalog::record_movement(
        &pool,
        RecordMovementInput {
            product_id: p_regular,
            reason: MovementReason::Sale,
            quantity: dec!(7),
            unit_cost: None,
            note: Some("sold".into()),
            actor_id: actor,
        },
    )
    .await
    .unwrap();

    let (st_stock, err_stock) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Too many units",
            "items": [{ "product_id": p_regular, "quantity": "4" }]
        })),
    )
    .await;
    assert_eq!(st_stock, StatusCode::CONFLICT);
    assert!(
        err_stock["detail"]
            .as_str()
            .unwrap()
            .contains("insufficient stock")
    );
    assert_eq!(
        catalog::get_product(&pool, p_regular).await.unwrap().stock,
        Some(dec!(3))
    );

    // Zero-cost promotional line returns cleanly with total 0.00 and no wallet/account rows.
    let (st_zero, ret_zero) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Return promo units",
            "items": [{ "product_id": p_promo, "quantity": "2" }]
        })),
    )
    .await;
    assert_eq!(st_zero, StatusCode::CREATED);
    assert_eq!(ret_zero["total"], "0.00");
    assert_eq!(ret_zero["debt_cancelled"], "0.00");
    assert_eq!(ret_zero["credit_granted"], "0.00");
    assert_eq!(ret_zero["refunds"].as_array().unwrap().len(), 0);
    assert_eq!(
        catalog::get_product(&pool, p_promo).await.unwrap().stock,
        Some(dec!(3))
    );

    // Deactivate supplier (balance == 0 && U == 0 -> allowed).
    // Leaving supplier_credit on inactive supplier is rejected with 409,
    // while wallet_refund on the historical cash purchase still succeeds.
    suppliers::deactivate_supplier(&pool, supplier)
        .await
        .unwrap();

    let (st_inact, _) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Credit on inactive supplier",
            "surplus_mode": "supplier_credit",
            "items": [{ "product_id": p_regular, "quantity": "1" }]
        })),
    )
    .await;
    assert_eq!(st_inact, StatusCode::CONFLICT);

    let (st_wal, _) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Cash refund on inactive supplier",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": p_regular, "quantity": "1" }]
        })),
    )
    .await;
    assert_eq!(st_wal, StatusCode::CREATED);
}

#[sqlx::test]
async fn concurrent_returns_serialize_on_purchase_and_idempotency_key(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-race@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-race@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Race Supplier").await;
    let product = seed_product(&pool, "Race Item").await;
    let (_wallet, method) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    let purchase_id = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(10.00))],
        &[(method, dec!(100.00))],
        dec!(0.00),
    )
    .await;

    // Race 1: two concurrent requests with the same idempotency_key for 4 units.
    let uri = format!("/api/v1/purchases/{purchase_id}/returns");
    let body = json!({
        "reason": "Idempotent race",
        "surplus_mode": "wallet_refund",
        "items": [{ "product_id": product, "quantity": "4" }],
        "idempotency_key": "race-key-1"
    });
    let (r1, r2) = tokio::join!(
        call_json(&app, Some(&cookie), "POST", &uri, Some(body.clone())),
        call_json(&app, Some(&cookie), "POST", &uri, Some(body)),
    );
    let statuses = [r1.0, r2.0];
    assert!(statuses.contains(&StatusCode::CREATED));
    assert!(statuses.contains(&StatusCode::OK));
    assert_eq!(r1.1["id"], r2.1["id"]);
    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(6))
    );

    // Race 2: two concurrent unkeyed requests each trying to return 4 of the remaining 6 units.
    // Exactly one succeeds (201) and one fails (409), leaving 2 units in stock.
    let body2 = json!({
        "reason": "Over-return race",
        "surplus_mode": "wallet_refund",
        "items": [{ "product_id": product, "quantity": "4" }]
    });
    let (c1, c2) = tokio::join!(
        call_json(&app, Some(&cookie), "POST", &uri, Some(body2.clone())),
        call_json(&app, Some(&cookie), "POST", &uri, Some(body2)),
    );
    let c_statuses = [c1.0, c2.0];
    assert!(c_statuses.contains(&StatusCode::CREATED));
    assert!(c_statuses.contains(&StatusCode::CONFLICT));
    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(2))
    );
}

#[sqlx::test]
async fn manual_stock_movement_rejects_purchase_return_reason(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-manual@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-manual@example.com", &pw).await;

    let product = seed_product(&pool, "Manual Guard Item").await;
    catalog::record_movement(
        &pool,
        RecordMovementInput {
            product_id: product,
            reason: MovementReason::Purchase,
            quantity: dec!(5),
            unit_cost: Some(dec!(10.00)),
            note: Some("seed".into()),
            actor_id: actor,
        },
    )
    .await
    .unwrap();

    let (status, body) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/products/{product}/movements"),
        Some(json!({
            "reason": "purchase_return",
            "quantity": "1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["detail"].as_str().unwrap().contains("purchase_return"));
    assert_eq!(
        catalog::get_product(&pool, product).await.unwrap().stock,
        Some(dec!(5))
    );
}

#[sqlx::test]
async fn rounding_drift_ceiling_rejects_return_exceeding_purchase_total(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-round@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-round@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Rounding Supplier").await;
    let product = seed_product(&pool, "Fractional Grain").await;
    let (_wallet, method) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    // 1.000 unit at 1.00 = 1.00 total. Returning 0.335 x 3 rounds each return
    // to 0.34 (0.34 + 0.34 + 0.34 = 1.02 > 1.00), so the third return must be
    // refused with 409 naming the remaining money allowance (0.32).
    let purchase_id = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(1.000), dec!(1.00))],
        &[(method, dec!(1.00))],
        dec!(0.00),
    )
    .await;

    for i in 1..=2 {
        let (st, ret) = call_json(
            &app,
            Some(&cookie),
            "POST",
            &format!("/api/v1/purchases/{purchase_id}/returns"),
            Some(json!({
                "reason": format!("Partial {i}"),
                "surplus_mode": "wallet_refund",
                "items": [{ "product_id": product, "quantity": "0.335" }]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(ret["total"], "0.34");
    }

    // 0.330 units left (0.33 > 0.32 remaining money allowance): refused with 409.
    let (st_over, err) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{purchase_id}/returns"),
        Some(json!({
            "reason": "Crosses money ceiling",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "0.330" }]
        })),
    )
    .await;
    assert_eq!(st_over, StatusCode::CONFLICT);
    assert!(err["detail"].as_str().unwrap().contains("0.32"));
}

#[sqlx::test]
async fn concurrent_purchase_return_and_supplier_payment_allocation_do_not_deadlock(pool: PgPool) {
    let (actor, pw) = seed_user(&pool, "ret-nodeadlock@example.com", Role::Manager).await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "ret-nodeadlock@example.com", &pw).await;

    let supplier = seed_supplier(&pool, "Deadlock Proof Supplier").await;
    let product = seed_product(&pool, "Bolt").await;
    let (_wallet, method) = seed_wallet_and_method(&pool, "Bank", "Wire").await;

    // 10 @ 10.00 = 100.00 fully on account.
    let purchase_id = complete_purchase(
        &pool,
        actor,
        supplier,
        &[(product, dec!(10), dec!(10.00))],
        &[],
        dec!(100.00),
    )
    .await;

    // Race a 40.00 supplier return (supplier_credit) against a 50.00 supplier
    // payment allocated to the SAME purchase: because purchase_returns locks
    // `purchases` FOR NO KEY UPDATE (compatible with FK `FOR KEY SHARE`), both
    // serialize cleanly without a 40P01 deadlock and preserve the ledger
    // invariant (`balance + U == outstanding_total + adjustments`).
    let ret_uri = format!("/api/v1/purchases/{purchase_id}/returns");
    let pay_uri = format!("/api/v1/suppliers/{supplier}/payments");
    let (ret_res, pay_res) = tokio::join!(
        call_json(
            &app,
            Some(&cookie),
            "POST",
            &ret_uri,
            Some(json!({
                "reason": "Concurrent return",
                "surplus_mode": "supplier_credit",
                "items": [{ "product_id": product, "quantity": "4" }]
            })),
        ),
        call_json(
            &app,
            Some(&cookie),
            "POST",
            &pay_uri,
            Some(json!({
                "payment_method_id": method,
                "amount": "50.00",
                "allocations": [{ "purchase_id": purchase_id, "amount": "50.00" }]
            })),
        ),
    );

    assert_eq!(
        ret_res.0,
        StatusCode::CREATED,
        "return failed: {:?}",
        ret_res
    );
    assert_eq!(
        pay_res.0,
        StatusCode::CREATED,
        "payment failed: {:?}",
        pay_res
    );

    let pos = supplier_accounts::account_position(&pool, supplier)
        .await
        .unwrap();
    assert_eq!(pos.balance, dec!(10.00));
    assert_eq!(pos.unapplied_credit, dec!(0.00));
    assert_eq!(pos.outstanding_total, dec!(10.00));
    assert!(pos.is_consistent());
}

#[sqlx::test(migrations = "./migrations")]
async fn purchase_return_rejects_quantity_above_numeric_ceiling_with_422(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "owner@pyme.com", Role::Owner).await;
    let (_wallet, method) = seed_wallet_and_method(&pool, "Cash", "Cash").await;
    let product = seed_product(&pool, "Flour").await;
    let supplier = seed_supplier(&pool, "Mill").await;
    let app = app(pool.clone());
    let cookie = login_cookie(&app, "owner@pyme.com", &password).await;

    let (draft, _) = purchases::open_draft(&pool, actor).await.unwrap();
    purchases::set_supplier(&pool, draft, Some(supplier))
        .await
        .unwrap();
    purchases::add_item(&pool, draft, product, dec!(5), dec!(10.00))
        .await
        .unwrap();
    purchases::complete(
        &pool,
        draft,
        actor,
        &[PurchasePaymentInput {
            payment_method_id: method,
            amount: dec!(50.00),
        }],
        Decimal::ZERO,
        None,
    )
    .await
    .unwrap();

    let (status, _) = call_json(
        &app,
        Some(&cookie),
        "POST",
        &format!("/api/v1/purchases/{draft}/returns"),
        Some(json!({
            "reason": "Too large",
            "surplus_mode": "wallet_refund",
            "items": [{ "product_id": product, "quantity": "1000000000" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
