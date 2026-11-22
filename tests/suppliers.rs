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
use organox::routes::router;
use organox::suppliers::service::{self, CreateSupplierInput, SupplierError, UpdateSupplierInput};
use organox::suppliers::validation::ValidationError;
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
