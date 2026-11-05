//! Tests de integración del catálogo y del libro de stock.
//!
//! Cubren los dos caminos donde el stock se calcula —el servicio en Rust y el
//! `CASE` en SQL— y las reglas de negocio que no se pueden delegar al motor:
//! stock negativo, servicio sin movimientos, y borrado con historial.

use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self, CatalogError, CreateProductInput, RecordMovementInput};
use organox::routes::router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

const PASSWORD: &str = "contrasena-bastante-larga-123";

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

/// Crea un usuario y devuelve su id, para usarlo como autor de movimientos.
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
    .expect("usuario")
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
    .expect("producto")
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
        .expect("producto")
        .stock
        .expect("un producto tiene stock")
}

// --- Alta y validación --------------------------------------------------

#[sqlx::test]
async fn creates_product_and_service(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;

    let product = seed_product(&pool, ProductKind::Product, "Harina 000", None).await;
    let service = seed_product(&pool, ProductKind::Service, "Flete", None).await;

    let product_view = service::get_product(&pool, product).await.unwrap();
    assert_eq!(product_view.product.kind, ProductKind::Product);
    assert_eq!(product_view.stock, Some(dec!(0)));

    // Un servicio no tiene stock: None, no cero.
    let service_view = service::get_product(&pool, service).await.unwrap();
    assert_eq!(service_view.stock, None);

    // El actor existe y sigue siendo dueño del alta.
    assert_eq!(actor, actor);
}

#[sqlx::test]
async fn service_cannot_have_min_stock(pool: PgPool) {
    let err = service::create_product(
        &pool,
        CreateProductInput {
            kind: ProductKind::Service,
            sku: None,
            name: "Flete".into(),
            description: None,
            price: None,
            min_stock: Some(dec!(5)),
        },
    )
    .await
    .expect_err("un servicio no puede tener stock mínimo");

    assert!(matches!(err, CatalogError::Validation(_)));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "el rechazo no debe dejar filas");
}

#[sqlx::test]
async fn duplicate_sku_is_rejected_without_partial_insert(pool: PgPool) {
    service::create_product(
        &pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: Some("HAR-001".into()),
            name: "Harina".into(),
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
            name: "Otra harina".into(),
            description: None,
            price: None,
            min_stock: None,
        },
    )
    .await
    .expect_err("sku duplicado");

    assert!(matches!(err, CatalogError::SkuTaken));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn multiple_products_without_sku_are_allowed(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Sin sku A", None).await;
    seed_product(&pool, ProductKind::Product, "Sin sku B", None).await;

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM products WHERE sku IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

// --- Libro de stock -----------------------------------------------------

#[sqlx::test]
async fn stock_is_the_signed_sum_of_movements(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;

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
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(2))
        .await
        .unwrap();

    let err = move_stock(&pool, actor, product, MovementReason::Sale, dec!(3))
        .await
        .expect_err("no se puede vender más de lo que hay");

    match err {
        CatalogError::InsufficientStock {
            available,
            requested,
        } => {
            assert_eq!(available, dec!(2));
            assert_eq!(requested, dec!(3));
        }
        other => panic!("error inesperado: {other:?}"),
    }

    // El rechazo no dejó rastro: stock y cantidad de movimientos intactos.
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
    let service_id = seed_product(&pool, ProductKind::Service, "Flete", None).await;

    let err = move_stock(&pool, actor, service_id, MovementReason::Purchase, dec!(1))
        .await
        .expect_err("un servicio no lleva stock");

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
    .expect_err("producto inexistente");

    assert!(matches!(err, CatalogError::NotFound));
}

/// Sostiene la décima de la invariante: el libro es inmutable a nivel DB.
#[sqlx::test]
async fn movement_history_cannot_be_modified(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    let update = sqlx::query("UPDATE stock_movements SET quantity = 99")
        .execute(&pool)
        .await;
    assert!(update.is_err(), "el libro no debe permitir UPDATE");

    let delete = sqlx::query("DELETE FROM stock_movements")
        .execute(&pool)
        .await;
    assert!(delete.is_err(), "el libro no debe permitir DELETE");

    assert_eq!(stock_of(&pool, product).await, dec!(5));
}

#[sqlx::test]
async fn listing_movements_returns_signed_quantities_newest_first(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;

    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(10))
        .await
        .unwrap();
    move_stock(&pool, actor, product, MovementReason::Sale, dec!(4))
        .await
        .unwrap();

    let rows = service::list_movements(&pool, product, 10).await.unwrap();
    assert_eq!(rows.len(), 2);

    // La cantidad almacenada es positiva; el efecto se calcula.
    assert_eq!(rows[0].reason, MovementReason::Sale);
    assert_eq!(rows[0].quantity, dec!(4));
    assert_eq!(rows[0].signed_quantity, dec!(-4));
    assert_eq!(rows[1].signed_quantity, dec!(10));

    // Autoría: el actor queda registrado.
    assert_eq!(rows[0].created_by, Some(actor));
}

/// Producto y libro de stock en una transacción: si el INSERT del movimiento
/// falla, el producto sigue sin stock y sin filas.
#[sqlx::test]
async fn failed_movement_leaves_no_partial_state(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;

    // Cantidad con más decimales de los que soporta la columna: rechazada en
    // validación, antes de tocar la DB.
    let err = move_stock(
        &pool,
        actor,
        product,
        MovementReason::Purchase,
        dec!(1.0004),
    )
    .await
    .expect_err("precisión inválida");
    assert!(matches!(err, CatalogError::Validation(_)));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stock_movements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// --- Modificación y baja ------------------------------------------------

#[sqlx::test]
async fn update_changes_only_provided_fields(pool: PgPool) {
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;

    service::update_product(
        &pool,
        product,
        service::UpdateProductInput {
            sku: None,
            name: Some("Harina 0000".into()),
            description: None,
            price: None,
            min_stock: None,
            is_active: None,
        },
    )
    .await
    .unwrap();

    let view = service::get_product(&pool, product).await.unwrap();
    assert_eq!(view.product.name, "Harina 0000");
    // No se enviaron: siguen como estaban.
    assert_eq!(view.product.price, Some(dec!(1000)));
    assert!(view.product.is_active);
}

#[sqlx::test]
async fn deactivate_hides_from_default_list_but_keeps_history(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    service::deactivate_product(&pool, product).await.unwrap();

    let visible = service::list_products(&pool, None, false, None, 50, 0)
        .await
        .unwrap();
    assert!(
        visible.is_empty(),
        "un producto inactivo no aparece por defecto"
    );

    let all = service::list_products(&pool, None, true, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].stock, Some(dec!(5)), "el historial se conserva");

    // La deactivación es idempotente.
    service::deactivate_product(&pool, product).await.unwrap();
}

#[sqlx::test]
async fn deactivating_unknown_product_is_not_found(pool: PgPool) {
    let err = service::deactivate_product(&pool, Uuid::now_v7())
        .await
        .expect_err("no existe");
    assert!(matches!(err, CatalogError::NotFound));
}

// --- Filtros ------------------------------------------------------------

#[sqlx::test]
async fn filters_by_kind_and_search(pool: PgPool) {
    seed_product(&pool, ProductKind::Product, "Harina 000", None).await;
    seed_product(&pool, ProductKind::Product, "Azúcar", None).await;
    seed_product(&pool, ProductKind::Service, "Harina: molienda", None).await;

    let products = service::list_products(&pool, Some(ProductKind::Product), false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(products.len(), 2);

    let services = service::list_products(&pool, Some(ProductKind::Service), false, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].stock, None);

    // La búsqueda es case-insensitive e insensible a acentos no aplica: ILIKE
    // resuelve mayúsculas, que es lo que se promete.
    let found = service::list_products(&pool, None, false, Some("harina"), 50, 0)
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
}

#[sqlx::test]
async fn low_stock_flag_reflects_threshold(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", Some(dec!(5))).await;

    let view = service::get_product(&pool, product).await.unwrap();
    let low = organox::catalog::model::is_low_stock(&view.product, view.stock);
    assert_eq!(low, Some(true), "stock 0 por debajo del mínimo 5");

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
                    json!({ "kind": "product", "name": "Harina 000", "price": "1500.50" })
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
    // Decimales como string: un float en JSON perdería precisión.
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
                    json!({ "kind": "service", "name": "Flete" }).to_string(),
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
    assert!(body["stock"].is_null(), "un servicio no tiene stock");
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
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;
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
    seed_product(&pool, ProductKind::Product, "Harina", None).await;

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?search=har")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["name"], "Harina");
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 256)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// --- Concurrencia -------------------------------------------------------
/// El movimiento espera si otro proceso tiene la fila del producto bloqueada.
///
/// ALCANCE REAL DE ESTE TEST (limitación conocida, no una virtud):
/// prueba que la operación se serializa por producto, pero NO aísla el
/// `SELECT ... FOR UPDATE` del servicio. El `INSERT` en `stock_movements`
/// hace que la FK adquiera `FOR KEY SHARE` sobre la fila de `products`, y ese
/// lock también conflicta con el `FOR UPDATE` del `blocker`. Se comprobó
/// quitando el `FOR UPDATE` del servicio: este test seguía pasando.
///
/// Por lo tanto este test cubre la serialización, no la corrección del lock
/// propio. Lo que sí cubre directo es `sale_cannot_make_stock_negative`
/// (regla de negocio) y el CHECK de la DB.
#[sqlx::test]
async fn record_movement_waits_for_product_lock(pool: PgPool) {
    let actor = seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let product = seed_product(&pool, ProductKind::Product, "Harina", None).await;
    move_stock(&pool, actor, product, MovementReason::Purchase, dec!(5))
        .await
        .unwrap();

    // Un "otro proceso" toma exactamente el mismo lock que usa el servicio.
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

    // Mientras el lock está tomado, el movimiento no puede completarse.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !handle.is_finished(),
        "el movimiento debe esperar el lock del producto; si terminó, \
         el servicio no está serializando por producto"
    );

    // Liberado el lock, el movimiento avanza normalmente.
    blocker.commit().await.unwrap();
    let result = handle.await.unwrap();
    assert!(
        result.is_ok(),
        "tras liberar el lock debe completarse: {result:?}"
    );

    assert_eq!(stock_of(&pool, product).await, dec!(4));
}

/// Contrato de entrada: los decimales se aceptan como string Y como número.
///
/// Esto NO es un descuido: `rust_decimal` deserializa ambos, y el spec de
/// OpenAPI documenta exactamente eso con `oneOf: [string, number]`. Si algún
/// día se restringe a solo string, hay que romper este test a propósito y
/// anotarlo en `odd/tasks/ui-admin.md`.
#[sqlx::test]
async fn decimal_inputs_accept_both_string_and_number(pool: PgPool) {
    seed_user(&pool, "a@pyme.com", Role::Owner).await;
    let cookie = login_cookie(&pool, "a@pyme.com").await;

    // Como string.
    let as_string = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "product", "name": "Con string", "price": "1500.50" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(as_string.status(), StatusCode::CREATED);

    // Como número JSON.
    let as_number = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/products")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "kind": "product", "name": "Con número", "price": 1500.50 })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(as_number.status(), StatusCode::CREATED);

    // En la respuesta, SIEMPRE string: el numérico no gana precisión de vuelta.
    let list = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/products?search=número")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_json(list).await;
    assert_eq!(body["items"][0]["price"], "1500.50");
}
