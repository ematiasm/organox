//! Purchases integration tests.
//!
//! Protects the full draft-to-confirm cash purchase lifecycle:
//!  * one open draft per operator;
//!  * goods only (services and inactive products rejected);
//!  * active supplier required on attach and on completion, name frozen at close;
//!  * transactional completion writes `stock_movements` (`purchase` + `unit_cost`)
//!    and `wallet_movements` (`purchase_payment`, subtracting from wallet balance);
//!  * split payments must equal server total; zero-total purchases accept empty payments;
//!  * completed and cancelled purchases are immutable (`409`);
//!  * paginated list with status/supplier filters and RFC 7807 HTTP contract.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self as auth_repo, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self as catalog, CreateProductInput};
use organox::payments::model::WalletMovementKind;
use organox::payments::repository::{self as wallet_repo, NewWalletMovement};
use organox::payments::service::{self as payments, CreateMethodInput, CreateWalletInput};
use organox::purchases::model::{PurchasePaymentInput, PurchaseStatus};
use organox::purchases::service::{self as purchases, PurchaseError};
use organox::routes::router;
use organox::supplier_accounts::model::SupplierAccountEntryKind;
use organox::supplier_accounts::repository::{self as supplier_accounts, NewSupplierPayment};
use organox::suppliers::service::{self as suppliers, CreateSupplierInput, UpdateSupplierInput};
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

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("valid json")
}

async fn seed_good(pool: &PgPool, name: &str) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(dec!(500.00)),
            min_stock: None,
        },
    )
    .await
    .expect("product")
}

async fn seed_service_product(pool: &PgPool, name: &str) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Service,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(dec!(500.00)),
            min_stock: None,
        },
    )
    .await
    .expect("service")
}

async fn seed_supplier(pool: &PgPool, name: &str) -> Uuid {
    seed_supplier_with_limit(pool, name, None).await
}

async fn seed_supplier_with_limit(
    pool: &PgPool,
    name: &str,
    credit_limit: Option<Decimal>,
) -> Uuid {
    suppliers::create_supplier(
        pool,
        CreateSupplierInput {
            name: name.to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit,
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

// --- Draft lifecycle and item rules -------------------------------------

#[sqlx::test]
async fn open_draft_reuses_the_operators_open_draft(pool: PgPool) {
    let (user_a, _) = seed_user(&pool, "a@organox.test", Role::Manager).await;
    let (user_b, _) = seed_user(&pool, "b@organox.test", Role::Manager).await;

    let (id1, reused1) = purchases::open_draft(&pool, user_a).await.unwrap();
    assert!(!reused1);

    let (id2, reused2) = purchases::open_draft(&pool, user_a).await.unwrap();
    assert_eq!(id1, id2);
    assert!(reused2);

    // Another operator gets their own draft.
    let (id_b, reused_b) = purchases::open_draft(&pool, user_b).await.unwrap();
    assert_ne!(id1, id_b);
    assert!(!reused_b);
}

#[sqlx::test]
async fn rejects_services_inactive_products_and_invalid_quantities_or_costs(pool: PgPool) {
    let (actor, _) = seed_user(&pool, "actor@organox.test", Role::Manager).await;
    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();
    let svc = seed_service_product(&pool, "Delivery").await;
    let good = seed_good(&pool, "Flour").await;

    let err = purchases::add_item(&pool, draft_id, svc, dec!(1), dec!(100.00))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::ServiceNotPurchasable));

    let err = purchases::add_item(&pool, draft_id, good, dec!(0), dec!(100.00))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::QuantityNotPositive));

    let err = purchases::add_item(&pool, draft_id, good, dec!(1.1234), dec!(100.00))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::QuantityPrecision));

    let err = purchases::add_item(&pool, draft_id, good, dec!(1), dec!(-1.00))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::UnitCostNegative));

    let err = purchases::add_item(&pool, draft_id, good, dec!(1), dec!(10.005))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::UnitCostPrecision));

    // Deactivated product cannot be added.
    catalog::deactivate_product(&pool, good).await.unwrap();
    let err = purchases::add_item(&pool, draft_id, good, dec!(2), dec!(50.00))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::ProductInactive));
}

#[sqlx::test]
async fn add_update_and_remove_items_recalculate_total(pool: PgPool) {
    let (actor, _) = seed_user(&pool, "actor@organox.test", Role::Manager).await;
    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();
    let flour = seed_good(&pool, "Flour").await;
    let sugar = seed_good(&pool, "Sugar").await;

    let p = purchases::add_item(&pool, draft_id, flour, dec!(2), dec!(150.00))
        .await
        .unwrap();
    assert_eq!(p.total(), dec!(300.00));

    // Adding same product sums quantity and updates unit_cost.
    let p = purchases::add_item(&pool, draft_id, flour, dec!(3), dec!(160.00))
        .await
        .unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].quantity, dec!(5));
    assert_eq!(p.items[0].unit_cost, dec!(160.00));
    assert_eq!(p.total(), dec!(800.00));

    // Adding a second product and updating via set_item.
    let p = purchases::add_item(&pool, draft_id, sugar, dec!(1.5), dec!(200.00))
        .await
        .unwrap();
    assert_eq!(p.total(), dec!(1100.00));

    let p = purchases::set_item(&pool, draft_id, sugar, dec!(2), dec!(210.00))
        .await
        .unwrap();
    assert_eq!(p.total(), dec!(1220.00));

    let p = purchases::remove_item(&pool, draft_id, flour)
        .await
        .unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.total(), dec!(420.00));
}

// --- Supplier attachment and completion ---------------------------------

#[sqlx::test]
async fn completes_a_purchase_atomically_adding_stock_and_deducting_wallets(pool: PgPool) {
    let (actor, _) = seed_user(&pool, "actor@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Molino Norte").await;
    let flour = seed_good(&pool, "Flour").await;
    let (cash_wallet, cash_method) = seed_wallet_and_method(&pool, "Cash Box", "Cash").await;
    let (bank_wallet, bank_method) = seed_wallet_and_method(&pool, "Bank", "Transfer").await;

    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();
    purchases::set_supplier(&pool, draft_id, Some(supplier_id))
        .await
        .unwrap();
    purchases::add_item(&pool, draft_id, flour, dec!(10), dec!(125.50))
        .await
        .unwrap();

    // Split cash + bank payment summing exactly to 1255.00.
    let completed = purchases::complete(
        &pool,
        draft_id,
        actor,
        &[
            PurchasePaymentInput {
                payment_method_id: cash_method,
                amount: dec!(500.00),
            },
            PurchasePaymentInput {
                payment_method_id: bank_method,
                amount: dec!(755.00),
            },
        ],
        Decimal::ZERO,
        Some("Invoice #42"),
    )
    .await
    .unwrap();

    assert_eq!(completed.purchase_id, draft_id);
    assert_eq!(completed.total, dec!(1255.00));
    assert_eq!(completed.movement_ids.len(), 1);
    assert_eq!(completed.payment_movement_ids.len(), 2);
    assert_eq!(completed.credit_applied, dec!(0.00));

    // Stock increased by 10 with reason=purchase and unit_cost=125.50.
    let product = catalog::get_product(&pool, flour).await.unwrap();
    assert_eq!(product.stock, Some(dec!(10)));
    let stock_moves = catalog::list_movements(&pool, flour, 10).await.unwrap();
    assert_eq!(stock_moves.len(), 1);
    assert_eq!(stock_moves[0].reason, MovementReason::Purchase);
    assert_eq!(stock_moves[0].quantity, dec!(10));
    assert_eq!(stock_moves[0].unit_cost, Some(dec!(125.50)));

    // Wallets deducted with purchase_payment linked to this purchase.
    let cash = payments::get_wallet(&pool, cash_wallet).await.unwrap();
    let bank = payments::get_wallet(&pool, bank_wallet).await.unwrap();
    assert_eq!(cash.balance, dec!(-500.00));
    assert_eq!(bank.balance, dec!(-755.00));

    let cash_moves = payments::list_movements(&pool, cash_wallet, 10)
        .await
        .unwrap();
    assert_eq!(cash_moves.len(), 1);
    assert_eq!(cash_moves[0].kind, WalletMovementKind::PurchasePayment);
    assert_eq!(cash_moves[0].purchase_id, Some(draft_id));
    assert_eq!(cash_moves[0].payment_method_id, Some(cash_method));

    // Renaming the supplier afterwards does NOT change the frozen snapshot.
    suppliers::update_supplier(
        &pool,
        supplier_id,
        UpdateSupplierInput {
            name: Some("Molino Norte Renamed".to_string()),
            tax_id: None,
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap();

    let closed = purchases::get_purchase(&pool, draft_id).await.unwrap();
    assert_eq!(closed.status, PurchaseStatus::Completed);
    assert_eq!(closed.supplier_name.as_deref(), Some("Molino Norte"));
    assert_eq!(closed.note.as_deref(), Some("Invoice #42"));

    // Completed purchase is immutable.
    assert!(matches!(
        purchases::add_item(&pool, draft_id, flour, dec!(1), dec!(10.00))
            .await
            .unwrap_err(),
        PurchaseError::NotEditable
    ));
    assert!(matches!(
        purchases::set_supplier(&pool, draft_id, None)
            .await
            .unwrap_err(),
        PurchaseError::NotEditable
    ));
    assert!(matches!(
        purchases::cancel(&pool, draft_id).await.unwrap_err(),
        PurchaseError::NotEditable
    ));
}

#[sqlx::test]
async fn completion_guards_supplier_items_and_payment_sum_without_partial_writes(pool: PgPool) {
    let (actor, _) = seed_user(&pool, "actor@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Vendor A").await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;
    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();

    // Missing supplier rejected.
    purchases::add_item(&pool, draft_id, flour, dec!(2), dec!(100.00))
        .await
        .unwrap();
    let err = purchases::complete(
        &pool,
        draft_id,
        actor,
        &[PurchasePaymentInput {
            payment_method_id: method_id,
            amount: dec!(200.00),
        }],
        Decimal::ZERO,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PurchaseError::SupplierRequired));

    // Attach supplier, then test payment mismatch leaves zero stock & wallet writes.
    purchases::set_supplier(&pool, draft_id, Some(supplier_id))
        .await
        .unwrap();
    let err = purchases::complete(
        &pool,
        draft_id,
        actor,
        &[PurchasePaymentInput {
            payment_method_id: method_id,
            amount: dec!(150.00),
        }],
        Decimal::ZERO,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PurchaseError::PaymentMismatch { .. }));
    assert_eq!(
        catalog::get_product(&pool, flour).await.unwrap().stock,
        Some(dec!(0))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(0)
    );

    // Deactivating the supplier before completion blocks completion.
    suppliers::deactivate_supplier(&pool, supplier_id)
        .await
        .unwrap();
    let err = purchases::complete(
        &pool,
        draft_id,
        actor,
        &[PurchasePaymentInput {
            payment_method_id: method_id,
            amount: dec!(200.00),
        }],
        Decimal::ZERO,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PurchaseError::SupplierInactive));

    // Cannot attach an inactive supplier to a draft either.
    let (draft2, _) = purchases::open_draft(&pool, actor).await.unwrap();
    assert_eq!(draft2, draft_id);
    let err = purchases::set_supplier(&pool, draft_id, Some(supplier_id))
        .await
        .unwrap_err();
    assert!(matches!(err, PurchaseError::SupplierInactive));
}

#[sqlx::test]
async fn zero_total_purchase_completes_with_empty_payments_and_cancel_closes_draft(pool: PgPool) {
    let (actor, _) = seed_user(&pool, "actor@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Sample Vendor").await;
    let flour = seed_good(&pool, "Sample Flour").await;

    let (draft_id, _) = purchases::open_draft(&pool, actor).await.unwrap();
    purchases::set_supplier(&pool, draft_id, Some(supplier_id))
        .await
        .unwrap();
    purchases::add_item(&pool, draft_id, flour, dec!(3), dec!(0.00))
        .await
        .unwrap();

    let completed = purchases::complete(&pool, draft_id, actor, &[], Decimal::ZERO, None)
        .await
        .unwrap();
    assert_eq!(completed.total, dec!(0.00));
    assert!(completed.payment_movement_ids.is_empty());
    assert_eq!(
        catalog::get_product(&pool, flour).await.unwrap().stock,
        Some(dec!(3))
    );

    // Next draft can be cancelled and leaves no stock trace.
    let (draft2, reused) = purchases::open_draft(&pool, actor).await.unwrap();
    assert!(!reused);
    purchases::cancel(&pool, draft2).await.unwrap();
    let cancelled = purchases::get_purchase(&pool, draft2).await.unwrap();
    assert_eq!(cancelled.status, PurchaseStatus::Cancelled);
}

// --- HTTP contract & paginated list -------------------------------------

#[sqlx::test]
async fn http_purchase_flow_and_paginated_list(pool: PgPool) {
    let (_, password) = seed_user(&pool, "mgr@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Distro Central").await;
    let flour = seed_good(&pool, "Flour").await;
    let (_wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;
    let router = app(pool);

    // Unauthenticated -> 401
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/purchases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let cookie = login_cookie(&router, "mgr@organox.test", &password).await;

    // POST /purchases/draft -> 201 { id, reused: false }
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/purchases/draft")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let draft = json_body(res).await;
    let purchase_id = draft["id"].as_str().unwrap().to_string();
    assert_eq!(draft["reused"], false);

    // Second POST /purchases/draft -> 200 { id, reused: true }
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/purchases/draft")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let draft2 = json_body(res).await;
    assert_eq!(draft2["id"], purchase_id);
    assert_eq!(draft2["reused"], true);

    // PUT /purchases/{id}/supplier -> 204
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/purchases/{purchase_id}/supplier"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "supplier_id": supplier_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // POST /purchases/{id}/items -> 200 PurchaseResponse
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{purchase_id}/items"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "product_id": flour,
                        "quantity": "4",
                        "unit_cost": "250.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    assert_eq!(body["total"], "1000.00");
    assert_eq!(body["supplier_name"], "Distro Central");

    // POST /purchases/{id}/complete -> 200
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{purchase_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [
                            { "payment_method_id": method_id, "amount": "1000.00" }
                        ],
                        "note": "Delivery morning"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let done = json_body(res).await;
    assert_eq!(done["total"], "1000.00");
    assert_eq!(done["credit_applied"], "0.00");

    // GET /purchases?status=completed -> 200 paginated
    let res = router
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/purchases?status=completed&supplier_id={supplier_id}"
                ))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list = json_body(res).await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["id"], purchase_id);
    assert_eq!(list["items"][0]["total"], "1000.00");
    assert_eq!(list["items"][0]["supplier_name"], "Distro Central");
}

#[sqlx::test]
async fn inactive_product_returns_409_on_add_and_complete_and_omitted_supplier_returns_422(
    pool: PgPool,
) {
    let (_, password) = seed_user(&pool, "probe-http@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Vendor Probe").await;
    let good1 = seed_good(&pool, "Oats").await;
    let good2 = seed_good(&pool, "Barley").await;
    let (_wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;
    let router = app(pool.clone());
    let cookie = login_cookie(&router, "probe-http@organox.test", &password).await;

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/purchases/draft")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let pid = json_body(res).await["id"].as_str().unwrap().to_string();

    // S10: adding an inactive product returns 409 Conflict (not 404).
    catalog::deactivate_product(&pool, good1).await.unwrap();
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{pid}/items"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "product_id": good1, "quantity": "2", "unit_cost": "10.00" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );

    // Attach supplier, then verify omitted `supplier_id` (`{}`) returns 422 and keeps supplier.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/purchases/{pid}/supplier"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "supplier_id": supplier_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/purchases/{pid}/supplier"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/purchases/{pid}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(res).await["supplier_id"], supplier_id.to_string());

    // S10: completing after deactivating a draft's product returns 409 with zero ledger writes.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{pid}/items"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "product_id": good2, "quantity": "2", "unit_cost": "10.00" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    catalog::deactivate_product(&pool, good2).await.unwrap();
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{pid}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [{ "payment_method_id": method_id, "amount": "20.00" }] })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        catalog::get_product(&pool, good2).await.unwrap().stock,
        Some(dec!(0))
    );
}

// --- On-account completion, credit limits, and auto-credit application ---

async fn prepare_http_draft(
    pool: &PgPool,
    actor: Uuid,
    supplier_id: Uuid,
    product_id: Uuid,
    quantity: Decimal,
    unit_cost: Decimal,
) -> Uuid {
    let (draft_id, _) = purchases::open_draft(pool, actor).await.unwrap();
    purchases::set_supplier(pool, draft_id, Some(supplier_id))
        .await
        .unwrap();
    purchases::add_item(pool, draft_id, product_id, quantity, unit_cost)
        .await
        .unwrap();
    draft_id
}

async fn prepay_supplier(
    pool: &PgPool,
    wallet_id: Uuid,
    method_id: Uuid,
    supplier_id: Uuid,
    amount: Decimal,
    actor: Uuid,
) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let mv_id = wallet_repo::insert_movement(
        &mut *tx,
        NewWalletMovement {
            wallet_id,
            kind: WalletMovementKind::SupplierAccountPayment,
            amount,
            sale_id: None,
            purchase_id: None,
            payment_method_id: Some(method_id),
            credit_note_id: None,
            customer_id: None,
            supplier_id: Some(supplier_id),
            note: Some("supplier advance"),
            created_by: Some(actor),
        },
    )
    .await
    .unwrap();
    let rec = supplier_accounts::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id,
            wallet_movement_id: mv_id,
            amount,
            allocations: &[],
            idempotency_key: None,
            note: Some("supplier advance"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    rec.entry_id
}

#[sqlx::test]
async fn pure_on_account_purchase_records_stock_and_payable_without_wallet_movement(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "acct-pure@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Credit Mill").await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, _method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    let draft_id =
        prepare_http_draft(&pool, actor, supplier_id, flour, dec!(4), dec!(100.00)).await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "acct-pure@organox.test", &password).await;

    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [],
                        "on_account": "400.00",
                        "note": "Net 30"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let done = json_body(res).await;
    assert_eq!(done["purchase_id"], draft_id.to_string());
    assert_eq!(done["total"], "400.00");
    assert_eq!(done["movement_ids"].as_array().unwrap().len(), 1);
    assert!(done["payment_movement_ids"].as_array().unwrap().is_empty());
    assert_eq!(done["credit_applied"], "0.00");

    // Stock inflow written.
    let product = catalog::get_product(&pool, flour).await.unwrap();
    assert_eq!(product.stock, Some(dec!(4)));
    let stock_moves = catalog::list_movements(&pool, flour, 10).await.unwrap();
    assert_eq!(stock_moves.len(), 1);
    assert_eq!(stock_moves[0].reason, MovementReason::Purchase);
    assert_eq!(stock_moves[0].unit_cost, Some(dec!(100.00)));

    // Zero wallet movements.
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(0.00)
    );
    assert!(
        payments::list_movements(&pool, wallet_id, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // purchase_charge in supplier_account_entries and derived supplier balance == total.
    let mut conn = pool.acquire().await.unwrap();
    let entries = supplier_accounts::list_entries_in(&mut conn, supplier_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, SupplierAccountEntryKind::PurchaseCharge);
    assert_eq!(entries[0].amount, dec!(400.00));
    assert_eq!(entries[0].purchase_id, Some(draft_id));

    let supplier = suppliers::get_supplier(&pool, supplier_id).await.unwrap();
    assert_eq!(supplier.balance, dec!(400.00));
    assert_eq!(supplier.available_credit, dec!(0.00));
}

#[sqlx::test]
async fn mixed_cash_and_on_account_purchase_writes_wallet_outflow_and_payable(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "acct-mixed@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Split Vendor").await;
    let sugar = seed_good(&pool, "Sugar").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Bank", "Transfer").await;

    let draft_id =
        prepare_http_draft(&pool, actor, supplier_id, sugar, dec!(10), dec!(100.00)).await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "acct-mixed@organox.test", &password).await;

    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [
                            { "payment_method_id": method_id, "amount": "350.00" }
                        ],
                        "on_account": "650.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let done = json_body(res).await;
    assert_eq!(done["total"], "1000.00");
    assert_eq!(done["payment_movement_ids"].as_array().unwrap().len(), 1);
    assert_eq!(done["credit_applied"], "0.00");

    // Stock inflow recorded.
    assert_eq!(
        catalog::get_product(&pool, sugar).await.unwrap().stock,
        Some(dec!(10))
    );

    // Wallet outflow for the cash portion only.
    let wallet = payments::get_wallet(&pool, wallet_id).await.unwrap();
    assert_eq!(wallet.balance, dec!(-350.00));
    let moves = payments::list_movements(&pool, wallet_id, 10)
        .await
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].kind, WalletMovementKind::PurchasePayment);
    assert_eq!(moves[0].amount, dec!(350.00));
    assert_eq!(moves[0].purchase_id, Some(draft_id));

    // Ledger purchase_charge for the on_account portion only.
    let mut conn = pool.acquire().await.unwrap();
    let entries = supplier_accounts::list_entries_in(&mut conn, supplier_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, SupplierAccountEntryKind::PurchaseCharge);
    assert_eq!(entries[0].amount, dec!(650.00));
    assert_eq!(entries[0].purchase_id, Some(draft_id));

    let supplier = suppliers::get_supplier(&pool, supplier_id).await.unwrap();
    assert_eq!(supplier.balance, dec!(650.00));
    assert_eq!(supplier.available_credit, dec!(0.00));
}

#[sqlx::test]
async fn on_account_purchase_enforces_credit_limit_atomically(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "acct-cap@organox.test", Role::Manager).await;
    let cash_only_supplier =
        seed_supplier_with_limit(&pool, "Cash Only Mill", Some(dec!(0.00))).await;
    let capped_supplier = seed_supplier_with_limit(&pool, "Capped Mill", Some(dec!(500.00))).await;
    let oats = seed_good(&pool, "Oats").await;
    let wheat = seed_good(&pool, "Wheat").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "acct-cap@organox.test", &password).await;

    // 1. Cash-only supplier (credit_limit == 0): on_account > 0 rejected with 409 and zero writes.
    let cash_draft = prepare_http_draft(
        &pool,
        actor,
        cash_only_supplier,
        oats,
        dec!(1),
        dec!(100.00),
    )
    .await;
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{cash_draft}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "100.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let problem = json_body(res).await;
    assert!(problem["detail"].as_str().unwrap().contains("0.00"));

    assert_eq!(
        catalog::get_product(&pool, oats).await.unwrap().stock,
        Some(dec!(0))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(0.00)
    );
    assert_eq!(
        purchases::get_purchase(&pool, cash_draft)
            .await
            .unwrap()
            .status,
        PurchaseStatus::Draft
    );
    {
        let mut conn = pool.acquire().await.unwrap();
        assert!(
            supplier_accounts::list_entries_in(&mut conn, cash_only_supplier)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // Same cash-only supplier still completes normally when paid in full upfront.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{cash_draft}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [{ "payment_method_id": method_id, "amount": "100.00" }],
                        "on_account": "0.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        catalog::get_product(&pool, oats).await.unwrap().stock,
        Some(dec!(1))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(-100.00)
    );

    // 2. Capped supplier (credit_limit == 500.00): 300.00 succeeds, then 200.01 fails (409),
    // leaving stock/wallet/ledger untouched, and exact boundary 200.00 succeeds.
    let p1 = prepare_http_draft(&pool, actor, capped_supplier, wheat, dec!(3), dec!(100.00)).await;
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{p1}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "300.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        suppliers::get_supplier(&pool, capped_supplier)
            .await
            .unwrap()
            .balance,
        dec!(300.00)
    );

    let p2 = prepare_http_draft(
        &pool,
        actor,
        capped_supplier,
        wheat,
        dec!(2.5),
        dec!(100.00),
    )
    .await;
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{p2}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [{ "payment_method_id": method_id, "amount": "49.99" }],
                        "on_account": "200.01"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let problem = json_body(res).await;
    assert!(problem["detail"].as_str().unwrap().contains("200.00"));

    // Stock, wallet, ledger, and draft status untouched by rejected completion.
    assert_eq!(
        catalog::get_product(&pool, wheat).await.unwrap().stock,
        Some(dec!(3))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(-100.00)
    );
    assert_eq!(
        suppliers::get_supplier(&pool, capped_supplier)
            .await
            .unwrap()
            .balance,
        dec!(300.00)
    );
    assert_eq!(
        purchases::get_purchase(&pool, p2).await.unwrap().status,
        PurchaseStatus::Draft
    );

    // Exact boundary (300.00 + 200.00 == 500.00) succeeds.
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{p2}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [{ "payment_method_id": method_id, "amount": "50.00" }],
                        "on_account": "200.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        suppliers::get_supplier(&pool, capped_supplier)
            .await
            .unwrap()
            .balance,
        dec!(500.00)
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(-150.00)
    );
    assert_eq!(
        catalog::get_product(&pool, wheat).await.unwrap().stock,
        Some(dec!(5.5))
    );
}

#[sqlx::test]
async fn on_account_validation_rejects_negative_precision_and_sum_mismatch(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "acct-val@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Validation Mill").await;
    let corn = seed_good(&pool, "Corn").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;

    let draft_id = prepare_http_draft(&pool, actor, supplier_id, corn, dec!(2), dec!(100.00)).await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "acct-val@organox.test", &password).await;

    let invalid_payloads = [
        // Negative on_account.
        json!({
            "payments": [{ "payment_method_id": method_id, "amount": "210.00" }],
            "on_account": "-10.00"
        }),
        // More than 2 decimal places.
        json!({
            "payments": [],
            "on_account": "200.005"
        }),
        // Sum mismatch (underpaid).
        json!({
            "payments": [{ "payment_method_id": method_id, "amount": "100.00" }],
            "on_account": "50.00"
        }),
        // Sum mismatch (overpaid).
        json!({
            "payments": [{ "payment_method_id": method_id, "amount": "100.00" }],
            "on_account": "150.00"
        }),
    ];

    for payload in invalid_payloads {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "expected 422 for payload {payload}"
        );
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/problem+json"
        );
    }

    // No side effects after rejected requests.
    assert_eq!(
        catalog::get_product(&pool, corn).await.unwrap().stock,
        Some(dec!(0))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(0.00)
    );
    assert_eq!(
        purchases::get_purchase(&pool, draft_id)
            .await
            .unwrap()
            .status,
        PurchaseStatus::Draft
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        supplier_accounts::list_entries_in(&mut conn, supplier_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn on_account_purchase_auto_applies_prepaid_supplier_credit_oldest_first(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "acct-credit@organox.test", Role::Manager).await;
    // Cap of 200.00 also verifies advance credit lowers exposure before authorization.
    let supplier_id = seed_supplier_with_limit(&pool, "Advance Mill", Some(dec!(200.00))).await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Bank", "Transfer").await;

    // Two unallocated prepayments: 150.00 (older) and 200.00 (newer) -> balance -350.00, unapplied 350.00.
    let pay1_entry = prepay_supplier(
        &pool,
        wallet_id,
        method_id,
        supplier_id,
        dec!(150.00),
        actor,
    )
    .await;
    let pay2_entry = prepay_supplier(
        &pool,
        wallet_id,
        method_id,
        supplier_id,
        dec!(200.00),
        actor,
    )
    .await;

    let pos_before = supplier_accounts::account_position(&pool, supplier_id)
        .await
        .unwrap();
    assert_eq!(pos_before.balance, dec!(-350.00));
    assert_eq!(pos_before.unapplied_credit, dec!(350.00));

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "acct-credit@organox.test", &password).await;

    // Purchase 1: 250.00 on account -> fully covered by oldest-first credit (150.00 from pay1 + 100.00 from pay2).
    let p1 = prepare_http_draft(&pool, actor, supplier_id, flour, dec!(5), dec!(50.00)).await;
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{p1}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "250.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let done1 = json_body(res).await;
    assert_eq!(done1["total"], "250.00");
    assert_eq!(done1["credit_applied"], "250.00");

    let allocs1: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry_id, amount FROM supplier_account_allocations WHERE purchase_id = $1 ORDER BY created_at ASC, id ASC",
    )
    .bind(p1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        allocs1,
        vec![(pay1_entry, dec!(150.00)), (pay2_entry, dec!(100.00))]
    );

    let pos_mid = supplier_accounts::account_position(&pool, supplier_id)
        .await
        .unwrap();
    assert_eq!(pos_mid.balance, dec!(-100.00));
    assert_eq!(pos_mid.unapplied_credit, dec!(100.00));
    assert_eq!(pos_mid.outstanding_total, dec!(0.00));
    assert!(pos_mid.is_consistent());

    // Purchase 2: 300.00 on account (exposure -100 + 300 = 200 == credit_limit) -> applies remaining 100.00 from pay2.
    let p2 = prepare_http_draft(&pool, actor, supplier_id, flour, dec!(6), dec!(50.00)).await;
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{p2}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "300.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let done2 = json_body(res).await;
    assert_eq!(done2["total"], "300.00");
    assert_eq!(done2["credit_applied"], "100.00");

    let allocs2: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry_id, amount FROM supplier_account_allocations WHERE purchase_id = $1 ORDER BY created_at ASC, id ASC",
    )
    .bind(p2)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(allocs2, vec![(pay2_entry, dec!(100.00))]);

    let pos_after = supplier_accounts::account_position(&pool, supplier_id)
        .await
        .unwrap();
    assert_eq!(pos_after.balance, dec!(200.00));
    assert_eq!(pos_after.unapplied_credit, dec!(0.00));
    assert_eq!(pos_after.outstanding_total, dec!(200.00));
    assert!(pos_after.is_consistent());

    let supplier = suppliers::get_supplier(&pool, supplier_id).await.unwrap();
    assert_eq!(supplier.balance, dec!(200.00));
    assert_eq!(supplier.available_credit, dec!(0.00));
}

#[sqlx::test]
async fn a_busy_supplier_account_answers_409_without_partial_writes(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "busy-supp@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Busy Mill").await;
    let flour = seed_good(&pool, "Flour").await;
    let draft_id = prepare_http_draft(&pool, actor, supplier_id, flour, dec!(2), dec!(50.00)).await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "busy-supp@organox.test", &password).await;

    // Another connection holds the supplier advisory lock (seed 1).
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 1))")
        .bind(supplier_id.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        router.oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "100.00" }).to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("completion never answered: the supplier lock wait is not bounded")
    .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["detail"], "the supplier account is busy, retry");

    holder.rollback().await.unwrap();

    // Nothing was written: purchase stays draft, stock stays 0, balance stays 0.
    assert_eq!(
        purchases::get_purchase(&pool, draft_id)
            .await
            .unwrap()
            .status,
        PurchaseStatus::Draft
    );
    assert_eq!(
        catalog::get_product(&pool, flour).await.unwrap().stock,
        Some(dec!(0))
    );
    let pos = supplier_accounts::account_position(&pool, supplier_id)
        .await
        .unwrap();
    assert_eq!(pos.balance, Decimal::ZERO);
    assert_eq!(pos.unapplied_credit, Decimal::ZERO);
}

#[sqlx::test]
async fn on_account_purchase_rejects_deactivated_supplier_without_partial_writes(pool: PgPool) {
    let (actor, password) = seed_user(&pool, "inactive-onacct@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Closed Mill").await;
    let flour = seed_good(&pool, "Flour").await;
    let draft_id = prepare_http_draft(&pool, actor, supplier_id, flour, dec!(2), dec!(50.00)).await;

    suppliers::deactivate_supplier(&pool, supplier_id)
        .await
        .unwrap();

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "inactive-onacct@organox.test", &password).await;

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "payments": [], "on_account": "100.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(
        body["detail"],
        "the supplier is deactivated and cannot be used for a purchase"
    );

    assert_eq!(
        purchases::get_purchase(&pool, draft_id)
            .await
            .unwrap()
            .status,
        PurchaseStatus::Draft
    );
    assert_eq!(
        catalog::get_product(&pool, flour).await.unwrap().stock,
        Some(dec!(0))
    );
    let pos = supplier_accounts::account_position(&pool, supplier_id)
        .await
        .unwrap();
    assert_eq!(pos.balance, Decimal::ZERO);
    assert_eq!(pos.unapplied_credit, Decimal::ZERO);
}

#[sqlx::test]
async fn cash_purchase_for_inactive_supplier_returns_409_even_with_invalid_payments(pool: PgPool) {
    let (actor, password) =
        seed_user(&pool, "inactive-cash-prec@organox.test", Role::Manager).await;
    let supplier_id = seed_supplier(&pool, "Closed Cash Mill").await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, method_id) = seed_wallet_and_method(&pool, "Cash", "Cash").await;
    let draft_id = prepare_http_draft(&pool, actor, supplier_id, flour, dec!(2), dec!(50.00)).await;

    suppliers::deactivate_supplier(&pool, supplier_id)
        .await
        .unwrap();

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "inactive-cash-prec@organox.test", &password).await;

    // 1. Deactivated supplier + payment sum mismatch -> 409 SupplierInactive (baseline precedence).
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [{ "payment_method_id": method_id, "amount": "10.00" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = json_body(res).await;
    assert_eq!(
        body["detail"],
        "the supplier is deactivated and cannot be used for a purchase"
    );

    // 2. Deactivated supplier + unknown/unavailable payment method -> 409 SupplierInactive.
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/purchases/{draft_id}/complete"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "payments": [{ "payment_method_id": Uuid::now_v7(), "amount": "100.00" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = json_body(res).await;
    assert_eq!(
        body["detail"],
        "the supplier is deactivated and cannot be used for a purchase"
    );

    // Rejected calls leave draft, stock, and wallet untouched.
    assert_eq!(
        purchases::get_purchase(&pool, draft_id)
            .await
            .unwrap()
            .status,
        PurchaseStatus::Draft
    );
    assert_eq!(
        catalog::get_product(&pool, flour).await.unwrap().stock,
        Some(dec!(0))
    );
    assert_eq!(
        payments::get_wallet(&pool, wallet_id)
            .await
            .unwrap()
            .balance,
        dec!(0.00)
    );
}
