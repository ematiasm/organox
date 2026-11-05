//! Tests de integración de ventas.
//!
//! Lo que estos tests protegen, en orden de importancia:
//!  1. Cobrar descuenta stock exactamente una vez.
//!  2. Un cobro sin stock suficiente no deja NADA (ni movimientos, ni venta
//!     cerrada, ni carrito modificado).
//!  3. Un borrador abandonado nunca toca el libro de stock.
//!  4. El total lo calcula el servidor.

use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::catalog::model::{MovementReason, ProductKind};
use organox::catalog::service::{self as catalog, CreateProductInput, RecordMovementInput};
use organox::routes::router;
use organox::sales::model::SaleStatus;
use organox::sales::service::{self as sales, SalesError};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

const PASSWORD: &str = "contrasena-bastante-larga-123";

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

async fn seed_user(pool: &PgPool) -> Uuid {
    let email = validation::ValidatedEmail::parse("cajero@pyme.com").expect("email");
    let hash = password::hash(PASSWORD).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("usuario")
}

async fn seed_product(pool: &PgPool, name: &str, price: Decimal) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Product,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(price),
            min_stock: None,
        },
    )
    .await
    .expect("producto")
}

async fn seed_service(pool: &PgPool, name: &str, price: Decimal) -> Uuid {
    catalog::create_product(
        pool,
        CreateProductInput {
            kind: ProductKind::Service,
            sku: None,
            name: name.to_string(),
            description: None,
            price: Some(price),
            min_stock: None,
        },
    )
    .await
    .expect("servicio")
}

async fn stock(pool: &PgPool, actor: Uuid, product: Uuid, qty: Decimal) {
    catalog::record_movement(
        pool,
        RecordMovementInput {
            product_id: product,
            reason: MovementReason::Purchase,
            quantity: qty,
            unit_cost: None,
            note: None,
            actor_id: actor,
        },
    )
    .await
    .expect("entrada de stock");
}

async fn current_stock(pool: &PgPool, product: Uuid) -> Decimal {
    catalog::get_product(pool, product)
        .await
        .expect("producto")
        .stock
        .expect("stock")
}

// --- Borrador -----------------------------------------------------------

/// Recargar la pantalla de venta no debe crear carritos nuevos.
#[sqlx::test]
async fn open_draft_reuses_the_open_one(pool: PgPool) {
    let actor = seed_user(&pool).await;

    let first = sales::open_draft(&pool, actor).await.unwrap();
    let second = sales::open_draft(&pool, actor).await.unwrap();

    assert_eq!(first, second, "debe devolver el mismo borrador abierto");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sales WHERE status = 'draft'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// Después de cobrar, el siguiente `open_draft` sí crea uno nuevo.
#[sqlx::test]
async fn open_draft_creates_new_after_completion(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    sales::complete(&pool, draft, actor).await.unwrap();

    let next = sales::open_draft(&pool, actor).await.unwrap();
    assert_ne!(draft, next, "tras cobrar, el siguiente borrador es nuevo");
}

// --- Ítems --------------------------------------------------------------

#[sqlx::test]
async fn adding_the_same_product_sums_quantity_and_freezes_price(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();
    let sale = sales::add_item(&pool, draft, product, dec!(3))
        .await
        .unwrap();

    assert_eq!(sale.item_count(), 1, "el mismo producto es una sola fila");
    assert_eq!(sale.items[0].quantity, dec!(5));
    // El precio viene del producto y queda congelado en el ítem.
    assert_eq!(sale.items[0].unit_price, dec!(1500.50));
    assert_eq!(sale.total(), dec!(7502.50));
}

/// Cambiar el precio del producto NO cambia el total del carrito abierto: el
/// ítem congeló el precio al agregarse.
#[sqlx::test]
async fn price_change_does_not_alter_an_open_draft(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    catalog::update_product(
        &pool,
        product,
        catalog::UpdateProductInput {
            sku: None,
            name: None,
            description: None,
            price: Some(dec!(999)),
            min_stock: None,
            is_active: None,
        },
    )
    .await
    .unwrap();

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.total(), dec!(200), "el precio se congeló al agregar");
}

#[sqlx::test]
async fn service_cannot_be_sold_as_item(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let service = seed_service(&pool, "Flete", dec!(8000)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let err = sales::add_item(&pool, draft, service, dec!(1))
        .await
        .expect_err("un servicio no lleva stock ni se vende como ítem");

    assert!(matches!(err, SalesError::ServiceNotSellable));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sale_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn quantity_must_be_positive(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    let err = sales::add_item(&pool, draft, product, dec!(0))
        .await
        .expect_err("cantidad 0");
    assert!(matches!(err, SalesError::QuantityNotPositive));
}

#[sqlx::test]
async fn set_quantity_zero_removes_the_item(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(2))
        .await
        .unwrap();

    let sale = sales::set_item_quantity(&pool, draft, product, dec!(0))
        .await
        .unwrap();
    assert_eq!(sale.item_count(), 0);
    assert_eq!(sale.total(), Decimal::ZERO);
}

// --- Cobro --------------------------------------------------------------

#[sqlx::test]
async fn completing_a_sale_discounts_stock_exactly_once(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(1500.50)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(3))
        .await
        .unwrap();
    let done = sales::complete(&pool, draft, actor).await.unwrap();

    assert_eq!(done.total, dec!(4501.50));
    assert_eq!(done.movement_ids.len(), 1);
    assert_eq!(current_stock(&pool, product).await, dec!(7));

    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Completed);
    assert!(sale.completed_at.is_some());

    // Un solo movimiento de salida: cobrar dos veces el mismo borrador no es
    // posible porque deja de ser editable.
    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 1);
}

#[sqlx::test]
async fn completing_twice_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    sales::complete(&pool, draft, actor).await.unwrap();

    let err = sales::complete(&pool, draft, actor)
        .await
        .expect_err("ya está cerrada");
    assert!(matches!(err, SalesError::NotEditable));

    // El stock se descontó una sola vez.
    assert_eq!(current_stock(&pool, product).await, dec!(9));
}

/// El test que más importa: sin stock, el cobro falla y **no deja nada**.
#[sqlx::test]
async fn insufficient_stock_leaves_no_state_change(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(2)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(5))
        .await
        .unwrap();

    let err = sales::complete(&pool, draft, actor)
        .await
        .expect_err("no hay stock suficiente");

    match err {
        SalesError::InsufficientStock {
            product_name,
            available,
            requested,
        } => {
            assert_eq!(product_name, "Harina");
            assert_eq!(available, dec!(2));
            assert_eq!(requested, dec!(5));
        }
        other => panic!("error inesperado: {other:?}"),
    }

    // Nada cambió: ni estado, ni stock, ni movimientos de venta.
    let sale = sales::get(&pool, draft).await.unwrap();
    assert_eq!(sale.status, SaleStatus::Draft, "sigue siendo borrador");
    assert_eq!(current_stock(&pool, product).await, dec!(2));

    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 0);
}

/// Caja A tiene que ver el borrador, no el stock descontado. Recién al cobrar
/// baja el stock y por lo tanto el borrador de B pasa a ser inválido.
#[sqlx::test]
async fn two_drafts_competing_for_the_last_units(pool: PgPool) {
    let actor_a = seed_user(&pool).await;
    let actor_b = {
        let email = validation::ValidatedEmail::parse("caja2@pyme.com").unwrap();
        let hash = password::hash(PASSWORD).unwrap();
        repository::create_user(
            &pool,
            NewUser {
                email: &email,
                password_hash: &hash,
                role: Role::Encargado,
            },
        )
        .await
        .unwrap()
    };

    let product = seed_product(&pool, "Última unidad", dec!(500)).await;
    stock(&pool, actor_a, product, dec!(1)).await;

    let draft_a = sales::open_draft(&pool, actor_a).await.unwrap();
    let draft_b = sales::open_draft(&pool, actor_b).await.unwrap();
    sales::add_item(&pool, draft_a, product, dec!(1))
        .await
        .unwrap();
    sales::add_item(&pool, draft_b, product, dec!(1))
        .await
        .unwrap();

    // A cobra: gana.
    sales::complete(&pool, draft_a, actor_a).await.unwrap();
    assert_eq!(current_stock(&pool, product).await, dec!(0));

    // B no puede cobrar: la unidad ya salió.
    let err = sales::complete(&pool, draft_b, actor_b)
        .await
        .expect_err("ya no queda stock");
    assert!(matches!(err, SalesError::InsufficientStock { .. }));
}

#[sqlx::test]
async fn empty_sale_cannot_be_completed(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let draft = sales::open_draft(&pool, actor).await.unwrap();

    let err = sales::complete(&pool, draft, actor)
        .await
        .expect_err("carrito vacío");
    assert!(matches!(err, SalesError::EmptySale));
}

/// Un borrador abandonado nunca tocó el libro de stock. Es la virtud de no
/// descontar hasta cobrar.
#[sqlx::test]
async fn abandoned_draft_never_touches_stock_ledger(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(4))
        .await
        .unwrap();
    sales::cancel(&pool, draft).await.unwrap();

    assert_eq!(current_stock(&pool, product).await, dec!(10));

    let sale_movements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stock_movements WHERE reason = 'sale'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_movements, 0);
}

#[sqlx::test]
async fn cancelling_a_completed_sale_is_rejected(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    let draft = sales::open_draft(&pool, actor).await.unwrap();
    sales::add_item(&pool, draft, product, dec!(1))
        .await
        .unwrap();
    sales::complete(&pool, draft, actor).await.unwrap();

    let err = sales::cancel(&pool, draft).await.expect_err("ya cobrada");
    assert!(matches!(err, SalesError::NotEditable));
}

#[sqlx::test]
async fn completed_sales_are_listed_newest_first(pool: PgPool) {
    let actor = seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(100)).await;
    stock(&pool, actor, product, dec!(10)).await;

    for _ in 0..2 {
        let draft = sales::open_draft(&pool, actor).await.unwrap();
        sales::add_item(&pool, draft, product, dec!(1))
            .await
            .unwrap();
        sales::complete(&pool, draft, actor).await.unwrap();
    }

    let list = sales::list_completed(&pool, 10).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.status == SaleStatus::Completed));
}

// --- HTTP ---------------------------------------------------------------

fn with_session(method: &str, uri: &str, cookie: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("session={cookie}"))
}

async fn login_cookie(pool: &PgPool) -> String {
    use tower::ServiceExt;
    let response = app(pool.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "email": "cajero@pyme.com", "password": PASSWORD })
                        .to_string(),
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
async fn http_sale_flow_returns_server_computed_total(pool: PgPool) {
    use tower::ServiceExt;

    seed_user(&pool).await;
    let product = seed_product(&pool, "Harina", dec!(1500.50)).await;
    // `query` sin macro: no necesita cache de `.sqlx/` y este test solo
    // necesita un id, no verificación de tipos de la consulta.
    let actor: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM users WHERE email = 'cajero@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    stock(&pool, actor, product, dec!(10)).await;

    let cookie = login_cookie(&pool).await;

    let draft = app(pool.clone())
        .oneshot(
            with_session("POST", "/api/v1/sales/draft", &cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        draft.status() == axum::http::StatusCode::CREATED
            || draft.status() == axum::http::StatusCode::OK
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(draft.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    let sale_id = body["id"].as_str().unwrap().to_string();

    let added = app(pool.clone())
        .oneshot(
            with_session("POST", &format!("/api/v1/sales/{sale_id}/items"), &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "product_id": product, "quantity": "2" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(added.status(), axum::http::StatusCode::OK);

    let sale: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(added.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();

    // Decimales como string, con el total calculado por el servidor.
    assert_eq!(sale["total"], "3001.00");
    assert_eq!(sale["item_count"], 1);
    assert_eq!(sale["items"][0]["subtotal"], "3001.00");
}
