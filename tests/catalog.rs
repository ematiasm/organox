//! Catalog and stock ledger integration tests.
//!
//! These cover both stock calculations, in the Rust service and SQL `CASE`,
//! and business rules that cannot be delegated to the engine: negative stock,
//! services without movements, and deletion with history.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app, seed_user_with_password};
use organox::auth::model::Role;
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self, CatalogError, CreateProductInput, RecordMovementInput};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn seed_user(pool: &PgPool, email: &str, role: Role) -> Uuid {
    seed_user_with_password(pool, email, role).await.0
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

async fn login_cookie(pool: &PgPool, email: &str, password: &str) -> String {
    let response = app(pool.clone())
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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;

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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;

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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;

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
async fn http_unknown_product_movements_returns_404(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{}/movements", Uuid::now_v7()))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // An unknown id is a missing product, not a product with no movements.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The other side of the same gate: an id that exists keeps answering `200`
/// with whatever history it has — `[]` when the product never moved, and the
/// movements of a deactivated product, because the ledger outlives the product.
#[sqlx::test]
async fn http_movements_of_an_existing_product_stay_200(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
    let never_moved = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    let deactivated = seed_product(&pool, ProductKind::Product, "Rice", None).await;
    move_stock(&pool, actor, deactivated, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();
    service::deactivate_product(&pool, deactivated)
        .await
        .unwrap();

    let empty = app(pool.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{never_moved}/movements"))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(
        body_json(empty).await.as_array().unwrap().len(),
        0,
        "a product that never moved has an empty history, not a missing one"
    );

    let history = app(pool)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/products/{deactivated}/movements"))
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history.status(), StatusCode::OK);
    assert_eq!(
        body_json(history).await.as_array().unwrap().len(),
        1,
        "deactivating a product must not hide its ledger"
    );
}

#[sqlx::test]
async fn http_insufficient_stock_returns_409_with_normalized_quantities(pool: PgPool) {
    let (actor, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(12.5))
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
                    json!({ "reason": "sale", "quantity": "20" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    // `numeric(12,3)` must not leak into the message: the operator reads the
    // same quantities the API returns, `12.5` and `20`, not `12.500`/`20.000`.
    let body = body_json(response).await;
    assert_eq!(
        body["detail"],
        "insufficient stock: available 12.5, requested 20"
    );
}

#[sqlx::test]
async fn http_list_returns_items_array(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
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

#[sqlx::test]
async fn http_list_echoes_clamped_limit_and_offset(pool: PgPool) {
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;
    seed_product(&pool, ProductKind::Product, "Flour", None).await;

    for (query, expected_limit, expected_offset) in [
        ("limit=9999&offset=-5", 200, 0),
        ("limit=0&offset=-1", 1, 0),
        ("limit=-10&offset=0", 1, 0),
    ] {
        let response = app(pool.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/products?{query}"))
                    .header("cookie", format!("session={cookie}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(
            body["limit"], expected_limit,
            "query {query} must echo clamped limit"
        );
        assert_eq!(
            body["offset"], expected_offset,
            "query {query} must echo clamped offset"
        );
        assert_eq!(body["total"], 1);
    }
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 256)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// --- Concurrency --------------------------------------------------------
/// The movement waits for the service's own `SELECT ... FOR UPDATE`.
///
/// The blocker deliberately takes `FOR KEY SHARE` instead of `FOR UPDATE`.
/// `FOR KEY SHARE` is the mode the `stock_movements` foreign key acquires
/// implicitly when its `INSERT` checks the parent row, and a lock mode never
/// conflicts with itself: with this blocker the FK check cannot wait, whatever
/// the service does. `FOR KEY SHARE` conflicts with exactly one request,
/// `FOR UPDATE`. So a backend waiting for this row can only be waiting on the
/// service's own lock, and removing that lock makes the wait disappear.
///
/// The wait is *observed* in `pg_locks` rather than assumed from a sleep: a
/// sleeping test would also pass if the worker had not started yet. `green`
/// here means another backend was seen waiting for a lock.
///
/// This lock is what makes two simultaneous sales of the last unit take
/// turns; `sale_cannot_make_stock_negative` covers the business rule and the
/// database CHECK on their own.
#[sqlx::test]
async fn record_movement_waits_for_product_lock(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Flour", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    // An "other process" holds a lock that only `FOR UPDATE` conflicts with.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM products WHERE id = $1 FOR KEY SHARE")
        .bind(product)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let worker_pool = pool.clone();
    let handle = tokio::spawn(async move {
        move_stock(&worker_pool, actor, product, MovementReason::Sale, dec!(1)).await
    });

    // Wait for the worker to be *seen* queued behind the product row lock. The
    // query runs on the blocker's connection: no third connection is needed,
    // and the blocker holds the only lock in this database (each `sqlx::test`
    // gets its own), so any other backend waiting on a lock is waiting on this
    // row.
    //
    // It does not filter on `locktype`. Postgres reports a row-lock wait as a
    // `tuple` lock only when the waiter has to queue behind another waiter;
    // a single waiter blocked by one holder waits on that holder's
    // `transactionid` instead. Filtering on either one of them would miss
    // half the cases.
    let mut observed: Option<String> = None;
    for _ in 0..100 {
        observed = sqlx::query_scalar(
            "SELECT locktype FROM pg_locks \
             WHERE NOT granted AND pid <> pg_backend_pid() \
             LIMIT 1",
        )
        .fetch_optional(&mut *blocker)
        .await
        .unwrap();
        if observed.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        observed.is_some(),
        "no backend was seen waiting for a lock after 5s, so the service never \
         asked for `FOR UPDATE`: the blocker holds `FOR KEY SHARE`, which the \
         foreign key also takes and which never conflicts with itself, so only \
         the service's own lock can wait here"
    );
    assert!(
        !handle.is_finished(),
        "the movement must not complete while the product lock is held"
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
    let (_, password) = seed_user_with_password(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com", &password).await;

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

/// Same rule as the customer search, in the other module: the wildcards an
/// operator types are text. Duplicated as a test on purpose — the two searches
/// share one escape helper, and a regression in either must be visible.
#[sqlx::test]
async fn search_treats_the_wildcards_literally(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Kiosco 50% Off", None).await;
    seed_product(&pool, ProductKind::Product, "50X Off", None).await;
    seed_product(&pool, ProductKind::Product, "under_score", None).await;
    seed_product(&pool, ProductKind::Product, "underXscore", None).await;

    let (percent, total) = service::list_products(&pool, None, false, Some("%"), 50, 0)
        .await
        .expect("search");
    assert_eq!(total, 1, "a bare % must not match every product");
    assert_eq!(percent[0].product.name, "Kiosco 50% Off");

    let (prefixed, _) = service::list_products(&pool, None, false, Some("50%"), 50, 0)
        .await
        .expect("search");
    assert_eq!(prefixed.len(), 1);
    assert_eq!(prefixed[0].product.name, "Kiosco 50% Off");

    let (underscore, _) = service::list_products(&pool, None, false, Some("_"), 50, 0)
        .await
        .expect("search");
    assert_eq!(underscore.len(), 1, "a bare _ must not match every name");
    assert_eq!(underscore[0].product.name, "under_score");

    let (plain, _) = service::list_products(&pool, None, false, Some("off"), 50, 0)
        .await
        .expect("search");
    assert_eq!(
        plain.len(),
        2,
        "an ordinary word still matches by substring"
    );
}

/// Same two pins as the customer search: the escape character itself, and the
/// re-count path, which no other assertion exercises with a metacharacter.
#[sqlx::test]
async fn search_finds_a_literal_backslash(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Ruta C:\\Proveedores", None).await;
    seed_product(&pool, ProductKind::Product, "Ruta C:/Proveedores", None).await;

    let (found, total) = service::list_products(&pool, None, false, Some("C:\\"), 50, 0)
        .await
        .expect("search");
    assert_eq!(
        total, 1,
        "a backslash is not a wildcard and not a separator"
    );
    assert_eq!(found[0].product.name, "Ruta C:\\Proveedores");
}

#[sqlx::test]
async fn a_page_past_the_end_recounts_with_the_escaped_pattern(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Kiosco 50% Off", None).await;
    seed_product(&pool, ProductKind::Product, "Plain Shop", None).await;

    let (empty, total) = service::list_products(&pool, None, false, Some("%"), 50, 99)
        .await
        .expect("page");
    assert!(empty.is_empty());
    assert_eq!(total, 1, "the re-count uses the escaped pattern too");
}

#[sqlx::test]
async fn http_catalog_rejects_values_above_numeric_column_ceilings_with_422(pool: PgPool) {
    let (_actor, password) = seed_user_with_password(&pool, "owner@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "owner@pyme.com", &password).await;

    // Price above numeric(12,2) -> 422, not 500.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "product",
                        "name": "Too Pricey",
                        "price": "10000000000.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // min_stock above numeric(12,3) -> 422, not 500.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "product",
                        "name": "Too Much Min Stock",
                        "min_stock": "1000000000"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Exact ceilings succeed on create (201).
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "product",
                        "name": "Max Boundary Item",
                        "price": "9999999999.99",
                        "min_stock": "999999999.999"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(&body).unwrap();
    let product_id = created["id"].as_str().unwrap();

    // Stock movement quantity above numeric(12,3) -> 422.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{product_id}/movements"))
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "reason": "purchase",
                        "quantity": "1000000000"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Stock movement unit_cost above numeric(12,2) -> 422.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{product_id}/movements"))
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "reason": "purchase",
                        "quantity": "1",
                        "unit_cost": "10000000000.00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // PATCH with price or min_stock above column ceilings -> 422 and stored values unchanged.
    for payload in [
        json!({ "price": "10000000000.00" }),
        json!({ "min_stock": "1000000000" }),
    ] {
        let res = app(pool.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/products/{product_id}"))
                    .header("cookie", format!("session={cookie}"))
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    let product_uuid: Uuid = product_id.parse().unwrap();
    let stored = service::get_product(&pool, product_uuid).await.unwrap();
    assert_eq!(stored.product.price, Some(dec!(9_999_999_999.99)));
    assert_eq!(stored.product.min_stock, Some(dec!(999_999_999.999)));
}

#[sqlx::test]
async fn manual_stock_movement_rejects_non_manual_reasons(pool: PgPool) {
    let (actor, password) =
        seed_user_with_password(&pool, "manual-ret@pyme.com", Role::Manager).await;
    let cookie = login_cookie(&pool, "manual-ret@pyme.com", &password).await;
    let product = seed_product(&pool, ProductKind::Product, "Guard Item", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    // Customer returns (`return`) must come through credit notes, never the
    // manual movement endpoint.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{product}/movements"))
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "reason": "return", "quantity": "2" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(res).await;
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("return"), "unexpected detail: {detail}");
    assert!(
        detail.contains("credit note"),
        "unexpected detail: {detail}"
    );
    assert!(
        !detail.contains("purchase_return"),
        "return rejection must not mention purchase_return: {detail}"
    );

    // Supplier returns (`purchase_return`) stay rejected with their own message.
    let res = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/products/{product}/movements"))
                .header("cookie", format!("session={cookie}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "reason": "purchase_return", "quantity": "1" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(res).await;
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("purchase_return"),
        "unexpected detail: {detail}"
    );

    // Neither rejection wrote a movement or changed stock.
    assert_eq!(stock_of(&pool, product).await, dec!(5));

    // Direct service calls also reject both reasons with `NonManualReason`.
    for reason in [MovementReason::Return, MovementReason::PurchaseReturn] {
        let err = move_stock(&pool, actor, product, reason, dec!(1))
            .await
            .expect_err("non-manual reason must be rejected");
        assert!(
            matches!(
                err,
                CatalogError::Validation(organox::catalog::validation::ValidationError::NonManualReason {
                    reason: rejected,
                }) if rejected == reason
            ),
            "unexpected error for {reason}: {err:?}"
        );
    }
    assert_eq!(stock_of(&pool, product).await, dec!(5));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE product_id = $1")
            .bind(product)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // Manual reasons keep working through the same HTTP endpoint.
    for (reason, quantity) in [
        ("purchase", "3"),
        ("sale", "2"),
        ("adjustment", "1"),
        ("loss", "1"),
    ] {
        let res = app(pool.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/products/{product}/movements"))
                    .header("cookie", format!("session={cookie}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "reason": reason, "quantity": quantity }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::CREATED,
            "manual reason {reason} should still succeed"
        );
    }
    // 5 + 3 - 2 + 1 - 1 = 6
    assert_eq!(stock_of(&pool, product).await, dec!(6));
}
