//! Suppliers integration tests.
//!
//! Protects the registry invariants: optional case-insensitive unique tax_id,
//! blank strings normalized away, deactivation instead of deletion, honest page
//! totals, LIKE wildcard escaping, and RFC 7807 HTTP contract.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::ProductKind;
use organox::catalog::service::{self as catalog, CreateProductInput};
use organox::payments::model::WalletMovementKind;
use organox::payments::repository::{self as wallet_repo, NewMethod, NewWalletMovement};
use organox::purchases::service as purchases_service;
use organox::routes::router;
use organox::supplier_accounts::repository::{
    self as supplier_accounts, NewSupplierCreditRefund, NewSupplierPayment,
};
use organox::suppliers::service::{self, CreateSupplierInput, SupplierError, UpdateSupplierInput};
use organox::suppliers::validation::ValidationError;
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

async fn seed_user_with_password(pool: &PgPool, email: &str, role: Role) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(&password).expect("hash");
    let id = repository::create_user(
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

fn new_supplier(name: &str) -> CreateSupplierInput {
    CreateSupplierInput {
        name: name.to_string(),
        tax_id: None,
        email: None,
        phone: None,
        credit_limit: None,
    }
}

async fn seed_supplier(pool: &PgPool, name: &str, tax_id: Option<&str>) -> Uuid {
    service::create_supplier(
        pool,
        CreateSupplierInput {
            name: name.to_string(),
            tax_id: tax_id.map(str::to_string),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("supplier")
}

// --- Creation and validation --------------------------------------------

#[sqlx::test]
async fn creates_a_supplier_and_trims_its_fields(pool: PgPool) {
    let id = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "  Distribuidora Norte  ".to_string(),
            tax_id: Some("  30-71234567-9 ".to_string()),
            email: Some(" ventas@norte.example ".to_string()),
            phone: Some(" 555-0199 ".to_string()),
            credit_limit: None,
        },
    )
    .await
    .expect("create");

    let supplier = service::get_supplier(&pool, id).await.expect("get");
    assert_eq!(supplier.id, id);
    assert_eq!(supplier.name, "Distribuidora Norte");
    assert_eq!(supplier.tax_id.as_deref(), Some("30-71234567-9"));
    assert_eq!(supplier.email.as_deref(), Some("ventas@norte.example"));
    assert_eq!(supplier.phone.as_deref(), Some("555-0199"));
    assert!(supplier.is_active);
}

#[sqlx::test]
async fn rejects_blank_name_and_invalid_email(pool: PgPool) {
    let err = service::create_supplier(&pool, new_supplier("   "))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SupplierError::Validation(ValidationError::NameBlank)
    ));

    let err = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Acme".to_string(),
            tax_id: None,
            email: Some("not-an-email".to_string()),
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SupplierError::Validation(ValidationError::EmailInvalid)
    ));
}

#[sqlx::test]
async fn tax_id_is_optional_and_case_insensitively_unique(pool: PgPool) {
    // Multiple suppliers without a tax_id are allowed, including blank strings.
    let first = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Walk-in Vendor A".to_string(),
            tax_id: Some("   ".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("first");
    let second = seed_supplier(&pool, "Walk-in Vendor B", None).await;
    assert_ne!(first, second);
    assert_eq!(
        service::get_supplier(&pool, first).await.unwrap().tax_id,
        None
    );

    // Non-empty tax_id is unique regardless of case.
    seed_supplier(&pool, "Alpha Supply", Some("ar-30-999")).await;
    let err = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Beta Supply".to_string(),
            tax_id: Some("AR-30-999".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SupplierError::TaxIdTaken));
}

// --- Updates and deactivation -------------------------------------------

#[sqlx::test]
async fn updates_only_sent_fields_and_rejects_duplicate_tax_id(pool: PgPool) {
    let first = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Original Co".to_string(),
            tax_id: Some("TAX-1".to_string()),
            email: Some("a@example.com".to_string()),
            phone: Some("111".to_string()),
            credit_limit: None,
        },
    )
    .await
    .unwrap();
    let second = seed_supplier(&pool, "Other Co", Some("TAX-2")).await;

    service::update_supplier(
        &pool,
        first,
        UpdateSupplierInput {
            name: Some("Renamed Co".to_string()),
            tax_id: None,
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap();

    let updated = service::get_supplier(&pool, first).await.unwrap();
    assert_eq!(updated.name, "Renamed Co");
    assert_eq!(updated.tax_id.as_deref(), Some("TAX-1"));
    assert_eq!(updated.email.as_deref(), Some("a@example.com"));
    assert_eq!(updated.phone.as_deref(), Some("111"));

    let err = service::update_supplier(
        &pool,
        second,
        UpdateSupplierInput {
            name: None,
            tax_id: Some("tax-1".to_string()),
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SupplierError::TaxIdTaken));
}

#[sqlx::test]
async fn deactivate_is_idempotent_and_returns_404_for_unknown(pool: PgPool) {
    let id = seed_supplier(&pool, "Temp Vendor", None).await;

    service::deactivate_supplier(&pool, id).await.unwrap();
    assert!(!service::get_supplier(&pool, id).await.unwrap().is_active);

    // Second call succeeds without error.
    service::deactivate_supplier(&pool, id).await.unwrap();

    let err = service::deactivate_supplier(&pool, Uuid::now_v7())
        .await
        .unwrap_err();
    assert!(matches!(err, SupplierError::NotFound));
}

// --- Listing, search, pagination and wildcard escaping ------------------

#[sqlx::test]
async fn list_paginates_honestly_filters_inactive_and_escapes_wildcards(pool: PgPool) {
    let a = seed_supplier(&pool, "Acme 100% Goods", Some("30-111")).await;
    let b = seed_supplier(&pool, "Beacon Supply", Some("30-222")).await;
    let c = seed_supplier(&pool, "Cedar Wholesale", Some("30-333")).await;
    service::deactivate_supplier(&pool, b).await.unwrap();

    // Default excludes inactive.
    let (active, total) = service::list_suppliers(&pool, false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(active.iter().map(|s| s.id).collect::<Vec<_>>(), vec![a, c]);

    // include_inactive shows all three.
    let (all, total) = service::list_suppliers(&pool, true, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(all.len(), 3);

    // Offset past the end still reports the real total.
    let (empty, total) = service::list_suppliers(&pool, false, None, 10, 100)
        .await
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(total, 2);

    // Search matches tax_id case-insensitively and escapes LIKE `%` wildcard.
    let (by_doc, total) = service::list_suppliers(&pool, false, Some("30-333"), 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(by_doc[0].id, c);

    let (by_percent, total) = service::list_suppliers(&pool, false, Some("100%"), 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(by_percent[0].id, a);
}

// --- HTTP contract ------------------------------------------------------

#[sqlx::test]
async fn http_endpoints_enforce_auth_and_rfc7807_errors(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "mgr@organox.test", Role::Manager).await;
    let router = app(pool);

    // Unauthenticated request returns 401.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/suppliers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let cookie = login_cookie(&router, "mgr@organox.test", &password).await;

    // POST /suppliers -> 201 { id }
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": "Molino Sur",
                        "tax_id": "30-55555555-5",
                        "email": "hola@molino.test",
                        "phone": "1234"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created = json_body(res).await;
    let id = created["id"].as_str().expect("id");

    // Duplicate tax_id -> 409 problem+json
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": "Duplicate",
                        "tax_id": "30-55555555-5"
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

    // GET /suppliers/{id} -> 200
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    assert_eq!(body["name"], "Molino Sur");
    assert_eq!(body["is_active"], true);

    // PATCH /suppliers/{id} -> 204
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "name": "Molino Sur SA" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // POST /suppliers/{id}/deactivate -> 204
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // GET /suppliers?include_inactive=true -> 200 { items, limit, offset, total }
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/suppliers?include_inactive=true&search=molino")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list = json_body(res).await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["name"], "Molino Sur SA");
    assert_eq!(list["items"][0]["is_active"], false);

    // Validation error -> 422
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "name": "   " }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown ID -> 404
    let res = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{}", Uuid::now_v7()))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn list_orders_suppliers_case_insensitively_by_lower_name(pool: PgPool) {
    for name in ["beta", "Alpha", "aardvark"] {
        service::create_supplier(
            &pool,
            CreateSupplierInput {
                name: name.to_string(),
                tax_id: None,
                email: None,
                phone: None,
                credit_limit: None,
            },
        )
        .await
        .unwrap();
    }

    let (items, total) = service::list_suppliers(&pool, false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 3);
    let names: Vec<&str> = items.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["aardvark", "Alpha", "beta"]);
}

// --- Credit limit, derived account position, and deactivation guard -----

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

async fn seed_method(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
    let wallet = wallet_repo::create_wallet(pool, &format!("{name} wallet"))
        .await
        .expect("wallet");
    let method = wallet_repo::create_method(
        pool,
        NewMethod {
            name,
            wallet_id: wallet,
        },
    )
    .await
    .expect("method");
    (wallet, method)
}

async fn complete_on_account_purchase(
    pool: &PgPool,
    actor: Uuid,
    supplier_id: Uuid,
    product_id: Uuid,
    total: Decimal,
) -> Uuid {
    let (draft_id, _) = purchases_service::open_draft(pool, actor)
        .await
        .expect("draft");
    purchases_service::set_supplier(pool, draft_id, Some(supplier_id))
        .await
        .expect("supplier");
    purchases_service::add_item(pool, draft_id, product_id, dec!(1), total)
        .await
        .expect("item");
    purchases_service::complete(pool, draft_id, actor, &[], total, None)
        .await
        .expect("complete");
    draft_id
}

async fn record_unallocated_supplier_payment(
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
            purchase_return_id: None,
            customer_id: None,
            supplier_id: Some(supplier_id),
            note: Some("advance payment"),
            created_by: Some(actor),
        },
    )
    .await
    .expect("wallet movement");
    let rec = supplier_accounts::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id,
            wallet_movement_id: mv_id,
            amount,
            allocations: &[],
            idempotency_key: None,
            note: Some("advance payment"),
            created_by: actor,
        },
    )
    .await
    .expect("record payment");
    tx.commit().await.unwrap();
    rec.entry_id
}

#[sqlx::test]
async fn credit_limit_creation_and_validation_via_http(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "limits@organox.test", Role::Manager).await;
    let router = app(pool.clone());
    let cookie = login_cookie(&router, "limits@organox.test", &password).await;

    // 1. Omitted credit_limit -> null (unlimited).
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "Unlimited Default" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let unlimited_default_id = json_body(res).await["id"].as_str().unwrap().to_string();

    // 2. Explicit null credit_limit -> null (unlimited).
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "Unlimited Explicit", "credit_limit": null }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let unlimited_explicit_id = json_body(res).await["id"].as_str().unwrap().to_string();

    // 3. Zero credit_limit -> "0.00" (cash only).
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "Cash Only Co", "credit_limit": "0" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let cash_only_id = json_body(res).await["id"].as_str().unwrap().to_string();

    // 4. Positive credit_limit -> formatted to two decimals.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "Capped Co", "credit_limit": "2500.50" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let capped_id = json_body(res).await["id"].as_str().unwrap().to_string();

    for (id, expected_limit) in [
        (&unlimited_default_id, Value::Null),
        (&unlimited_explicit_id, Value::Null),
        (&cash_only_id, json!("0.00")),
        (&capped_id, json!("2500.50")),
    ] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/suppliers/{id}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_body(res).await;
        assert_eq!(body["credit_limit"], expected_limit);
        assert_eq!(body["balance"], "0.00");
        assert_eq!(body["available_credit"], "0.00");
    }

    // 5. Reject negative, 3-decimal, and oversized credit_limit with 422 problem+json.
    for bad_limit in ["-0.01", "10.005", "1000000000000.00"] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/suppliers")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "name": "Bad Limit", "credit_limit": bad_limit }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "expected 422 for credit_limit={bad_limit}"
        );
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/problem+json"
        );
    }

    // Rejected requests created no extra suppliers.
    let (_, total) = service::list_suppliers(&pool, true, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 4);
}

#[sqlx::test]
async fn patch_credit_limit_tri_state_via_http(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "patch@organox.test", Role::Manager).await;
    let router = app(pool);
    let cookie = login_cookie(&router, "patch@organox.test", &password).await;

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "TriState Vendor", "credit_limit": "1000.00" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let id = json_body(res).await["id"].as_str().unwrap().to_string();

    // Omitted credit_limit leaves existing cap untouched.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "TriState Vendor Renamed" }).to_string(),
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
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(res).await;
    assert_eq!(body["name"], "TriState Vendor Renamed");
    assert_eq!(body["credit_limit"], "1000.00");

    // Value updates cap (including 0 = cash only).
    for (patch_val, expected) in [("250.25", json!("250.25")), ("0", json!("0.00"))] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/suppliers/{id}"))
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "credit_limit": patch_val }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/suppliers/{id}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(res).await["credit_limit"], expected);
    }

    // Explicit null clears cap back to unlimited.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "credit_limit": null }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(res).await["credit_limit"], Value::Null);

    // Invalid credit_limit on PATCH returns 422 and leaves limit unchanged.
    for bad in ["-1.00", "5.123", "1000000000000.00"] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/suppliers/{id}"))
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "credit_limit": bad }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let res = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(res).await["credit_limit"], Value::Null);
}

#[sqlx::test]
async fn get_and_list_suppliers_return_derived_balance_and_available_credit(pool: PgPool) {
    let (actor, password) =
        seed_user_with_password(&pool, "derived@organox.test", Role::Manager).await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, method_id) = seed_method(&pool, "Bank").await;

    let alpha_id = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Alpha Mills".to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: Some(dec!(500.00)),
        },
    )
    .await
    .unwrap();
    let beta_id = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Beta Farms".to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap();

    // Alpha: 300.00 on-account purchase + 100.00 unallocated advance -> balance 200.00, available_credit 100.00.
    complete_on_account_purchase(&pool, actor, alpha_id, flour, dec!(300.00)).await;
    record_unallocated_supplier_payment(&pool, wallet_id, method_id, alpha_id, dec!(100.00), actor)
        .await;

    // Beta: 150.00 unallocated advance, no purchases -> balance -150.00, available_credit 150.00.
    record_unallocated_supplier_payment(&pool, wallet_id, method_id, beta_id, dec!(150.00), actor)
        .await;

    let router = app(pool);
    let cookie = login_cookie(&router, "derived@organox.test", &password).await;

    // GET /suppliers/{id}
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{alpha_id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let alpha = json_body(res).await;
    assert_eq!(alpha["credit_limit"], "500.00");
    assert_eq!(alpha["balance"], "200.00");
    assert_eq!(alpha["available_credit"], "100.00");

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/suppliers/{beta_id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let beta = json_body(res).await;
    assert_eq!(beta["credit_limit"], Value::Null);
    assert_eq!(beta["balance"], "-150.00");
    assert_eq!(beta["available_credit"], "150.00");

    // GET /suppliers
    let res = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/suppliers")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list = json_body(res).await;
    assert_eq!(list["total"], 2);
    assert_eq!(list["items"][0]["id"], alpha_id.to_string());
    assert_eq!(list["items"][0]["credit_limit"], "500.00");
    assert_eq!(list["items"][0]["balance"], "200.00");
    assert_eq!(list["items"][0]["available_credit"], "100.00");
    assert_eq!(list["items"][1]["id"], beta_id.to_string());
    assert_eq!(list["items"][1]["credit_limit"], Value::Null);
    assert_eq!(list["items"][1]["balance"], "-150.00");
    assert_eq!(list["items"][1]["available_credit"], "150.00");
}

#[sqlx::test]
async fn deactivation_guard_blocks_open_balance_or_unapplied_credit_and_allows_once_settled(
    pool: PgPool,
) {
    let (actor, password) =
        seed_user_with_password(&pool, "guard@organox.test", Role::Manager).await;
    let flour = seed_good(&pool, "Flour").await;
    let (wallet_id, method_id) = seed_method(&pool, "Cash").await;

    let debtor_id = seed_supplier(&pool, "Debtor Supplier", None).await;
    let prepaid_id = seed_supplier(&pool, "Prepaid Supplier", None).await;

    // Debtor owes 200.00 on an open on-account purchase.
    let purchase_id =
        complete_on_account_purchase(&pool, actor, debtor_id, flour, dec!(200.00)).await;

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "guard@organox.test", &password).await;

    // 1. Non-zero balance blocks both POST /deactivate and PATCH is_active=false with 409.
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{debtor_id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{debtor_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "is_active": false }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert!(
        service::get_supplier(&pool, debtor_id)
            .await
            .unwrap()
            .is_active
    );

    // 2. Zero balance BUT non-zero unapplied credit (unallocated payment matching charge)
    // still blocks both deactivation paths with 409.
    let payment_entry_id = record_unallocated_supplier_payment(
        &pool,
        wallet_id,
        method_id,
        debtor_id,
        dec!(200.00),
        actor,
    )
    .await;
    let pos = supplier_accounts::account_position(&pool, debtor_id)
        .await
        .unwrap();
    assert_eq!(pos.balance, dec!(0.00));
    assert_eq!(pos.unapplied_credit, dec!(200.00));

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{debtor_id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{debtor_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "is_active": false }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(
        service::get_supplier(&pool, debtor_id)
            .await
            .unwrap()
            .is_active
    );

    // 3. Allocate payment -> balance == 0 and unapplied_credit == 0 -> POST /deactivate succeeds (204).
    let mut tx = pool.begin().await.unwrap();
    supplier_accounts::allocate(&mut tx, payment_entry_id, purchase_id, dec!(200.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{debtor_id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(
        !service::get_supplier(&pool, debtor_id)
            .await
            .unwrap()
            .is_active
    );

    // 4. Pure unapplied advance (balance < 0, unapplied_credit > 0) blocks both endpoints,
    // and once refunded back to zero, PATCH is_active=false succeeds (204).
    record_unallocated_supplier_payment(
        &pool,
        wallet_id,
        method_id,
        prepaid_id,
        dec!(120.00),
        actor,
    )
    .await;

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{prepaid_id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{prepaid_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "is_active": false }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Refund the 120.00 advance so position is fully zeroed.
    let mut tx = pool.begin().await.unwrap();
    let refund_mv = wallet_repo::insert_movement(
        &mut *tx,
        NewWalletMovement {
            wallet_id,
            kind: WalletMovementKind::SupplierAccountRefund,
            amount: dec!(120.00),
            sale_id: None,
            purchase_id: None,
            payment_method_id: Some(method_id),
            credit_note_id: None,
            purchase_return_id: None,
            customer_id: None,
            supplier_id: Some(prepaid_id),
            note: Some("advance returned"),
            created_by: Some(actor),
        },
    )
    .await
    .unwrap();
    supplier_accounts::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: prepaid_id,
            wallet_movement_id: refund_mv,
            amount: dec!(120.00),
            idempotency_key: None,
            note: Some("advance returned"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let res = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{prepaid_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "is_active": false }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(
        !service::get_supplier(&pool, prepaid_id)
            .await
            .unwrap()
            .is_active
    );
}

#[sqlx::test]
async fn deactivation_routes_answer_409_when_supplier_account_is_busy(pool: PgPool) {
    let (_actor, password) =
        seed_user_with_password(&pool, "busy-deact@organox.test", Role::Manager).await;
    let supplier_id = service::create_supplier(
        &pool,
        CreateSupplierInput {
            name: "Locked Supplier".into(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .unwrap();

    let router = app(pool.clone());
    let cookie = login_cookie(&router, "busy-deact@organox.test", &password).await;

    // Hold the per-supplier advisory lock (seed 1) on another connection.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 1))")
        .bind(supplier_id.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        router.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/suppliers/{supplier_id}/deactivate"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("POST /deactivate never answered: lock wait is not bounded")
    .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = json_body(res).await;
    assert_eq!(body["detail"], "the supplier account is busy, retry");

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        router.oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/suppliers/{supplier_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "is_active": false }).to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("PATCH is_active=false never answered: lock wait is not bounded")
    .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = json_body(res).await;
    assert_eq!(body["detail"], "the supplier account is busy, retry");

    holder.rollback().await.unwrap();

    assert!(
        service::get_supplier(&pool, supplier_id)
            .await
            .unwrap()
            .is_active
    );
}

#[sqlx::test]
async fn credit_limit_check_constraint_rejects_negative_values_at_database_layer(pool: PgPool) {
    let insert_err = sqlx::query(
        "INSERT INTO suppliers (id, name, credit_limit) VALUES ($1, 'Negative Limit', -0.01)",
    )
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await
    .unwrap_err();

    match insert_err {
        sqlx::Error::Database(db) => {
            assert_eq!(db.code().as_deref(), Some("23514"));
            assert_eq!(db.constraint(), Some("suppliers_credit_limit_not_negative"));
        }
        other => panic!("expected DB CHECK violation on INSERT, got {other:?}"),
    }

    let supplier_id = seed_supplier(&pool, "Valid Limit", None).await;
    let update_err = sqlx::query("UPDATE suppliers SET credit_limit = -1.00 WHERE id = $1")
        .bind(supplier_id)
        .execute(&pool)
        .await
        .unwrap_err();

    match update_err {
        sqlx::Error::Database(db) => {
            assert_eq!(db.code().as_deref(), Some("23514"));
            assert_eq!(db.constraint(), Some("suppliers_credit_limit_not_negative"));
        }
        other => panic!("expected DB CHECK violation on UPDATE, got {other:?}"),
    }

    assert_eq!(
        service::get_supplier(&pool, supplier_id)
            .await
            .unwrap()
            .credit_limit,
        None
    );
}
