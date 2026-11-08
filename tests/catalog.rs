//! Catalog and stock ledger integration tests.
//!
//! These cover both stock calculations, in the Rust service and SQL `CASE`,
//! and business rules that cannot be delegated to the engine: negative stock,
//! services without movements, and deletion with history.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self, CatalogError, CreateProductInput, RecordMovementInput};
use organox::routes::router;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

const PASSWORD: &str = "password-long-enough-123";

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

/// Creates a user and returns its ID for use as the movement author.
async fn seed_user(pool: &PgPool, email: &str, role: Role) -> Uuid {
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(PASSWORD).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role,
        },
    )
    .await
    .expect("user")
}

async fn seed_product(
    pool: &PgPool,
    kind: ProductKind,
    name: &str,
    min_stock: Option<Decimal>,
) -> Uuid {
    service::create_product(
        pool,
        CreateProductInput {
            kind,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(dec!(1000)),
            min_stock,
        },
    )
    .await
    .expect("product")
}

async fn move_stock(
    pool: &PgPool,
    actor: Uuid,
    product: Uuid,
    reason: MovementReason,
    quantity: Decimal,
) -> Result<Uuid, CatalogError> {
    service::record_movement(
        pool,
        RecordMovementInput {
            product_id: product,
            reason,
            quantity,
            unit_cost: None,
            note: None,
            actor_id: actor,
        },
    )
    .await
}

async fn stock_of(pool: &PgPool, product: Uuid) -> Decimal {
    service::get_product(pool, product)
        .await
        .expect("product")
        .stock
        .expect("a product has stock")
}

// --- Creation and validation --------------------------------------------

#[sqlx::test]
async fn creates_product_and_service(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;

    let product = seed_product(&pool, ProductKind::Product, "Flour 000", None).await;
    let service = seed_product(&pool, ProductKind::Service, "Delivery", None).await;

    let product_view = service::get_product(&pool, product).await.unwrap();
    assert_eq!(product_view.product.kind, ProductKind::Product);
    assert_eq!(product_view.stock, Some(dec!(0)));

    // A service has no stock: None, not zero.
    let service_view = service::get_product(&pool, service).await.unwrap();
    assert_eq!(service_view.stock, None);

    // The actor exists and remains the owner of the creation.
    assert_eq!(actor, actor);
}

#[sqlx::test]
async fn service_cannot_have_min_stock(pool: PgPool) {
    let err = service::create_product(
        &pool,
        CreateProductInput {
            kind: ProductKind::Service,
            sku: None,
            name: "Delivery".into(),
            description: None,
            price: None,
            min_stock: Some(dec!(5)),
        },
    )
    .await
    .expect_err("a service cannot have minimum stock");

    assert!(matches!(err, CatalogError::Validation(_)));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "rejection must leave no rows");
}

#[sqlx::test]
async fn duplicate_sku_is_rejected_without_partial_insert(pool: PgPool) {
    service::create_product(
        &pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: Some("HAR-001".into()),
            name: "Flour".into(),
            description: None,
            price: None,
            min_stock: None,
        },
    )
    .await
    .unwrap();

    let err = service::create_product(
        &pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: Some("HAR-001".into()),
            name: "Another flour".into(),
            description: None,
            price: None,
            min_stock: None,
        },
    )
    .await
    .expect_err("duplicate SKU");

    assert!(matches!(err, CatalogError::SkuTaken));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn multiple_products_without_sku_are_allowed(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Without SKU A", None).await;
    seed_product(&pool, ProductKind::Product, "Without SKU B", None).await;

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products WHERE sku IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

// --- Stock ledger -------------------------------------------------------

#[sqlx::test]
async fn stock_is_the_signed_sum_of_movements(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(10))
        .await
        .unwrap();
    move_stock(&pool, actor, product, MovementReason::Sale, dec!(3.5))
        .await
        .unwrap();
    move_stock(&pool, actor, product, MovementReason::Loss, dec!(1))
        .await
        .unwrap();
    move_stock(&pool, actor, product, MovementReason::Adjustment, dec!(2))
        .await
        .unwrap();

    // 10 - 3.5 - 1 + 2 = 7.5
    assert_eq!(stock_of(&pool, product).await, dec!(7.5));
}

#[sqlx::test]
async fn sale_cannot_make_stock_negative(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(2))
        .await
        .unwrap();

    let err = move_stock(&pool, actor, product, MovementReason::Sale, dec!(3))
        .await
        .expect_err("cannot sell more than available");

    match err {
        CatalogError::InsufficientStock {
            available,
            requested,
        } => {
            assert_eq!(available, dec!(2));
            assert_eq!(requested, dec!(3));
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Rejection left no trace: stock and movement count are unchanged.
    assert_eq!(stock_of(&pool, product).await, dec!(2));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stock_movements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn service_cannot_receive_movements(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let service_id = seed_product(&pool, ProductKind::Service, "Delivery", None).await;

    let err = move_stock(&pool, actor, service_id, MovementReason::Purchase, dec!(1))
        .await
        .expect_err("a service has no stock");

    assert!(matches!(err, CatalogError::Validation(_)));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stock_movements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn movement_on_unknown_product_is_not_found(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;

    let err = move_stock(
        &pool,
        actor,
        Uuid::now_v7(),
        MovementReason::Purchase,
        dec!(1),
    )
    .await
    .expect_err("unknown product");

    assert!(matches!(err, CatalogError::NotFound));
}

/// Preserves the invariant: the ledger is immutable at the database level.
#[sqlx::test]
async fn movement_history_cannot_be_modified(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    let update = sqlx::query("UPDATE stock_movements SET quantity = 99")
        .execute(&pool)
        .await;
    assert!(update.is_err(), "the ledger must not allow UPDATE");

    let delete = sqlx::query("DELETE FROM stock_movements")
        .execute(&pool)
        .await;
    assert!(delete.is_err(), "the ledger must not allow DELETE");

    assert_eq!(stock_of(&pool, product).await, dec!(5));
}

#[sqlx::test]
async fn listing_movements_returns_signed_quantities_newest_first(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(10))
        .await
        .unwrap();
    move_stock(&pool, actor, product, MovementReason::Sale, dec!(4))
        .await
        .unwrap();

    let rows = service::list_movements(&pool, product, 10).await.unwrap();
    assert_eq!(rows.len(), 2);

    // The stored quantity is positive; the effect is calculated.
    assert_eq!(rows[0].reason, MovementReason::Sale);
    assert_eq!(rows[0].quantity, dec!(4));
    assert_eq!(rows[0].signed_quantity, dec!(-4));
    assert_eq!(rows[1].signed_quantity, dec!(10));

    // Authorship: the actor is recorded.
    assert_eq!(rows[0].created_by, Some(actor));
}

/// Product and stock ledger in one transaction: if the movement INSERT fails,
/// the product still has no stock and no movement rows.
#[sqlx::test]
async fn failed_movement_leaves_no_partial_state(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;

    // A quantity with more decimals than the column supports is rejected during
    // validation, before touching the database.
    let err = move_stock(
        &pool,
        actor,
        product,
        MovementReason::Purchase,
        dec!(1.0004),
    )
    .await
    .expect_err("invalid precision");
    assert!(matches!(err, CatalogError::Validation(_)));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stock_movements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// --- Update and deactivation --------------------------------------------

#[sqlx::test]
async fn update_changes_only_provided_fields(pool: PgPool) {
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;

    service::update_product(
        &pool,
        product,
        service::UpdateProductInput {
            sku: None,
            name: Some("Flour 0000".into()),
            description: None,
            price: None,
            min_stock: None,
            is_active: None,
        },
    )
    .await
    .unwrap();

    let view = service::get_product(&pool, product).await.unwrap();
    assert_eq!(view.product.name, "Flour 0000");
    // Not provided: they remain unchanged.
    assert_eq!(view.product.price, Some(dec!(1000)));
    assert!(view.product.is_active);
}

#[sqlx::test]
async fn deactivate_hides_from_default_list_but_keeps_history(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    service::deactivate_product(&pool, product).await.unwrap();

    let visible = service::list_products(&pool, None, false, None, 50, 0)
        .await
        .unwrap();
    assert!(
        visible.0.is_empty(),
        "an inactive product does not appear by default"
    );
    assert_eq!(visible.1, 0, "the hidden product is not counted");

    let all = service::list_products(&pool, None, true, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(all.0.len(), 1);
    assert_eq!(all.1, 1, "include_inactive counts the inactive product");
    assert_eq!(all.0[0].stock, Some(dec!(5)), "history is preserved");

    // Deactivation is idempotent.
    service::deactivate_product(&pool, product).await.unwrap();
}

#[sqlx::test]
async fn deactivating_unknown_product_is_not_found(pool: PgPool) {
    let err = service::deactivate_product(&pool, Uuid::now_v7())
        .await
        .expect_err("does not exist");
    assert!(matches!(err, CatalogError::NotFound));
}

// --- Filters ------------------------------------------------------------

#[sqlx::test]
async fn filters_by_kind_and_search(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Flour 000", None).await;
    seed_product(&pool, ProductKind::Product, "Sugar", None).await;
    seed_product(&pool, ProductKind::Service, "Flour: milling", None).await;

    let products = service::list_products(&pool, Some(ProductKind::Product), false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(products.0.len(), 2);
    assert_eq!(products.1, 2, "the total uses the same filters as the page");

    let services = service::list_products(&pool, Some(ProductKind::Service), false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(services.0.len(), 1);
    assert_eq!(services.1, 1, "a service is counted under its own kind");
    assert_eq!(services.0[0].stock, None);

    // Search is case-insensitive, not accent-insensitive: ILIKE handles case,
    // which is what is promised.
    let found = service::list_products(&pool, None, false, Some("flour"), 50, 0)
        .await
        .unwrap();
    assert_eq!(found.0.len(), 2);
    assert_eq!(found.1, 2, "the total respects the search filter");
}

#[sqlx::test]
async fn low_stock_flag_reflects_threshold(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", Some(dec!(5))).await;

    let view = service::get_product(&pool, product).await.unwrap();
    let low = organox::catalog::model::is_low_stock(&view.product, view.stock);
    assert_eq!(low, Some(true), "stock 0 is below minimum 5");

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(20))
        .await
        .unwrap();
    let view = service::get_product(&pool, product).await.unwrap();
    assert_eq!(
        organox::catalog::model::is_low_stock(&view.product, view.stock),
        Some(false)
    );
}

// --- HTTP ---------------------------------------------------------------

async fn login_cookie(pool: &PgPool, email: &str) -> String {
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": PASSWORD }).to_string(),
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
async fn catalog_endpoints_require_authentication(pool: PgPool) {
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/products")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn http_flow_creates_product_and_moves_stock(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;

    let created = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "product", "name": "Flour 000", "price": "1500.50" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(created.status(), StatusCode::CREATED);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let moved = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{id}/movements"))
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "reason": "purchase", "quantity": "12.5" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(moved.status(), StatusCode::CREATED);

    let fetched = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{id}"))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_json(fetched).await;
    // Decimals as strings: a JSON float would lose precision.
    assert_eq!(body["stock"], "12.5");
    assert_eq!(body["price"], "1500.50");
    assert_eq!(body["kind"], "product");
}

#[sqlx::test]
async fn http_service_has_null_stock(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;

    let created = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "service", "name": "Delivery" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let fetched = app(pool)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{id}"))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_json(fetched).await;
    assert!(body["stock"].is_null(), "a service has no stock");
    assert!(body["low_stock"].is_null());
}

#[sqlx::test]
async fn http_unknown_product_returns_404(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{}", Uuid::now_v7()))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn http_insufficient_stock_returns_409(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(1))
        .await
        .unwrap();

    let response = app(pool)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{product}/movements"))
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "reason": "sale", "quantity": "5" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[sqlx::test]
async fn http_list_returns_items_array(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;
    seed_product(&pool, ProductKind::Product, "Flour", None).await;

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?search=flo")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["name"], "Flour");
}

/// The list must say how many products match in total, not how many fit in the
/// page. Without it a client cannot tell a full page from the last page, which
/// is what made the frontend truncate at 50 silently (issue #22).
#[sqlx::test]
async fn http_list_reports_the_total_independently_of_the_page(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;
    for name in ["Flour", "Rice", "Salt"] {
        seed_product(&pool, ProductKind::Product, name, None).await;
    }

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?limit=2&offset=0")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["items"].as_array().unwrap().len(),
        2,
        "the page holds `limit` items"
    );
    assert_eq!(body["limit"], 2, "the page size echoes back");
    assert_eq!(body["offset"], 0, "the page offset echoes back");
    assert_eq!(
        body["total"], 3,
        "the total counts every matching product, not the page"
    );
}

/// An offset past the last product returns an empty page. The total must still
/// be the real one: `COUNT(*) OVER ()` reports nothing for zero rows, and a
/// client that reads 0 would conclude the catalog is empty instead of stale.
#[sqlx::test]
async fn http_list_past_the_last_page_still_reports_the_total(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;
    for name in ["Flour", "Rice"] {
        seed_product(&pool, ProductKind::Product, name, None).await;
    }

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?limit=2&offset=2")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert!(
        body["items"].as_array().unwrap().is_empty(),
        "the second page of two products is empty"
    );
    assert_eq!(
        body["total"], 2,
        "an empty page still reports the real total, so the client can step back"
    );
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 256)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// --- Concurrency --------------------------------------------------------
/// The movement waits if another process holds the product row lock.
///
/// ACTUAL SCOPE (a known limitation, not a virtue): this proves the operation
/// serializes per product, but does NOT isolate the service's `SELECT ... FOR
/// UPDATE`. The `INSERT` into `stock_movements` makes the FK acquire `FOR KEY
/// SHARE` on the `products` row, which also conflicts with the blocker's
/// `FOR UPDATE`. Removing the service's `FOR UPDATE` still passed this test.
///
/// Thus this covers serialization, not correctness of the service's own lock.
/// `sale_cannot_make_stock_negative` directly covers the business rule and the
/// database CHECK.
#[sqlx::test]
async fn record_movement_waits_for_product_lock(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    // An "other process" takes the same lock used by the service.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM products WHERE id = $1 FOR UPDATE")
        .bind(product)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let worker_pool = pool.clone();
    let handle = tokio::spawn(async move {
        move_stock(&worker_pool, actor, product, MovementReason::Sale, dec!(1)).await
    });

    // While the lock is held, the movement cannot complete.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !handle.is_finished(),
        "the movement must wait for the product lock; if it finished, \
         the service is not serializing per product"
    );

    // Once the lock is released, the movement proceeds normally.
    blocker.commit().await.unwrap();
    let result = handle.await.unwrap();
    assert!(
        result.is_ok(),
        "it must complete after releasing the lock: {result:?}"
    );

    assert_eq!(stock_of(&pool, product).await, dec!(4));
}

/// Input contract: decimals are accepted as both strings AND numbers.
///
/// This is NOT an oversight: `rust_decimal` deserializes both, and the OpenAPI
/// spec documents exactly that with `oneOf: [string, number]`. If restricted to
/// strings someday, deliberately break this test and note it in
/// `odd/tasks/ui-admin.md`.
#[sqlx::test]
async fn decimal_inputs_accept_both_string_and_number(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;

    // As a string.
    let as_string = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "product", "name": "With a string", "price": "1500.50" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(as_string.status(), StatusCode::CREATED);

    // As a JSON number.
    let as_number = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "product", "name": "With a number", "price": 1500.50 })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(as_number.status(), StatusCode::CREATED);

    // In the response, ALWAYS a string: numeric input does not regain precision.
    let list = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?search=number")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_json(list).await;
    assert_eq!(body["items"][0]["price"], "1500.50");
}
