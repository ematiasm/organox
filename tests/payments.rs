//! Payments integration tests.
//!
//! What these protect, in order of importance:
//!  1. The balance is the signed sum of the movements, always.
//!  2. A rejected movement leaves no trace: no row, no balance change.
//!  3. Repointing a method never moves money that was already collected.
//!  4. The ledger cannot be rewritten.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::payments::model::WalletMovementKind;
use organox::payments::repository::{self as ledger, NewWalletMovement};
use organox::routes::router;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;
use uuid::Uuid;

const EMAIL: &str = "owner@pyme.com";

/// A password generated per test run.
///
/// Not a literal on purpose: `rust/hard-coded-cryptographic-value` flags any
/// fixed value used as a credential, and it is right to — the habit is what
/// leaks into production code. The other test suites still carry that alert
/// (25 open instances of the same rule); this file does not add to the pile.
fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

async fn seed_owner(pool: &PgPool, password: &str) {
    let email = validation::ValidatedEmail::parse(EMAIL).expect("email");
    let hash = password::hash(password).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("user");
}

async fn login(pool: &PgPool) -> String {
    let password = test_password();
    seed_owner(pool, &password).await;
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": EMAIL, "password": password }).to_string(),
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
        .to_string()
}

/// One HTTP round trip. Returns the status and the parsed body (if any).
async fn call(
    pool: &PgPool,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }

    let request = builder
        .body(
            body.map(|body| Body::from(body.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();

    let response = app(pool.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn create_wallet(pool: &PgPool, cookie: &str, name: &str) -> Uuid {
    let (status, body) = call(
        pool,
        cookie,
        "POST",
        "/api/v1/wallets",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn create_method(pool: &PgPool, cookie: &str, name: &str, wallet: Uuid) -> Uuid {
    let (status, body) = call(
        pool,
        cookie,
        "POST",
        "/api/v1/payment-methods",
        Some(json!({ "name": name, "wallet_id": wallet })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn record(pool: &PgPool, cookie: &str, wallet: Uuid, kind: &str, amount: &str) -> StatusCode {
    let (status, _) = call(
        pool,
        cookie,
        "POST",
        &format!("/api/v1/wallets/{wallet}/movements"),
        Some(json!({ "kind": kind, "amount": amount })),
    )
    .await;
    status
}

async fn balance(pool: &PgPool, cookie: &str, wallet: Uuid) -> String {
    let (status, body) = call(
        pool,
        cookie,
        "GET",
        &format!("/api/v1/wallets/{wallet}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["balance"].as_str().unwrap().to_string()
}

async fn movement_count(pool: &PgPool, wallet: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM wallet_movements WHERE wallet_id = $1")
        .bind(wallet)
        .fetch_one(pool)
        .await
        .unwrap()
}

// --- Authentication -----------------------------------------------------

#[sqlx::test]
async fn payments_endpoints_require_a_session(pool: PgPool) {
    for uri in ["/api/v1/wallets", "/api/v1/payment-methods"] {
        let response = app(pool.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "GET {uri}");
    }
}

// --- Wallets ------------------------------------------------------------

/// "Caja" and "caja" are the same wallet; two of them would split the balance.
#[sqlx::test]
async fn a_duplicate_wallet_name_is_rejected_ignoring_case(pool: PgPool) {
    let cookie = login(&pool).await;
    create_wallet(&pool, &cookie, "Caja").await;

    let (status, body) = call(
        &pool,
        &cookie,
        "POST",
        "/api/v1/wallets",
        Some(json!({ "name": "caja" })),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "the name is already in use");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM wallets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the rejected wallet must not be stored");
}

#[sqlx::test]
async fn a_blank_wallet_name_is_rejected(pool: PgPool) {
    let cookie = login(&pool).await;

    let (status, body) = call(
        &pool,
        &cookie,
        "POST",
        "/api/v1/wallets",
        Some(json!({ "name": "   " })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "the name must not be blank");
}

#[sqlx::test]
async fn a_wallet_can_be_deactivated_and_is_hidden_unless_requested(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    let (status, _) = call(
        &pool,
        &cookie,
        "PATCH",
        &format!("/api/v1/wallets/{wallet}"),
        Some(json!({ "is_active": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, active) = call(&pool, &cookie, "GET", "/api/v1/wallets", None).await;
    assert_eq!(active.as_array().unwrap().len(), 0);

    let (_, all) = call(
        &pool,
        &cookie,
        "GET",
        "/api/v1/wallets?include_inactive=true",
        None,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["is_active"], false);
    // The wallet is still there. Nothing is deleted.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM wallets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn an_unknown_wallet_is_not_found(pool: PgPool) {
    let cookie = login(&pool).await;
    let missing = Uuid::now_v7();

    let (get_status, _) = call(
        &pool,
        &cookie,
        "GET",
        &format!("/api/v1/wallets/{missing}"),
        None,
    )
    .await;
    assert_eq!(get_status, StatusCode::NOT_FOUND);

    let (movement_status, _) = call(
        &pool,
        &cookie,
        "POST",
        &format!("/api/v1/wallets/{missing}/movements"),
        Some(json!({ "kind": "expense", "amount": "10.00" })),
    )
    .await;
    assert_eq!(movement_status, StatusCode::NOT_FOUND);

    let (method_status, _) = call(
        &pool,
        &cookie,
        "POST",
        "/api/v1/payment-methods",
        Some(json!({ "name": "Cash", "wallet_id": missing })),
    )
    .await;
    assert_eq!(
        method_status,
        StatusCode::NOT_FOUND,
        "a method needs a real wallet"
    );
}

// --- Balance ------------------------------------------------------------

/// The balance is the signed sum, not a counter someone has to remember to
/// update.
#[sqlx::test]
async fn balance_is_the_signed_sum_of_the_movements(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    assert_eq!(balance(&pool, &cookie, wallet).await, "0.00");

    assert_eq!(
        record(&pool, &cookie, wallet, "adjustment_in", "250.00").await,
        StatusCode::CREATED
    );
    assert_eq!(
        record(&pool, &cookie, wallet, "expense", "100.00").await,
        StatusCode::CREATED
    );

    assert_eq!(balance(&pool, &cookie, wallet).await, "150.00");

    let (_, movements) = call(
        &pool,
        &cookie,
        "GET",
        &format!("/api/v1/wallets/{wallet}/movements"),
        None,
    )
    .await;
    let rows = movements.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first, with the sign already applied for the client.
    assert_eq!(rows[0]["kind"], "expense");
    assert_eq!(rows[0]["amount"], "100.00");
    assert_eq!(rows[0]["signed_amount"], "-100.00");
    assert_eq!(rows[1]["signed_amount"], "250.00");
}

/// Recording what happened is not the same as allowing it: an expense is
/// accepted even if the wallet does not hold that much, and the negative
/// balance is the honest answer.
#[sqlx::test]
async fn an_expense_can_leave_a_wallet_negative(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    assert_eq!(
        record(&pool, &cookie, wallet, "expense", "50.00").await,
        StatusCode::CREATED
    );

    assert_eq!(balance(&pool, &cookie, wallet).await, "-50.00");
}

/// A collection enters the ledger only by charging a sale: that is the only
/// path that also deducts stock and freezes a total.
#[sqlx::test]
async fn a_sale_payment_cannot_be_typed_by_hand(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    let (status, body) = call(
        &pool,
        &cookie,
        "POST",
        &format!("/api/v1/wallets/{wallet}/movements"),
        Some(json!({ "kind": "sale_payment", "amount": "100.00" })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["detail"],
        "a sale payment is recorded by charging a sale, not by hand"
    );
    assert_eq!(movement_count(&pool, wallet).await, 0);
    assert_eq!(balance(&pool, &cookie, wallet).await, "0.00");
}

#[sqlx::test]
async fn a_zero_amount_is_rejected_and_nothing_is_written(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    assert_eq!(
        record(&pool, &cookie, wallet, "expense", "0.00").await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        record(&pool, &cookie, wallet, "expense", "-5.00").await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(movement_count(&pool, wallet).await, 0);
}

// --- Payment methods ----------------------------------------------------

/// A new method is a row. This test is the whole point of not using an enum:
/// nothing here needs a migration.
#[sqlx::test]
async fn a_payment_method_shows_its_wallet_and_can_be_repointed(pool: PgPool) {
    let cookie = login(&pool).await;
    let cash_box = create_wallet(&pool, &cookie, "Caja").await;
    let bank = create_wallet(&pool, &cookie, "Banco").await;

    let method = create_method(&pool, &cookie, "Cash", cash_box).await;

    let (_, list) = call(&pool, &cookie, "GET", "/api/v1/payment-methods", None).await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["wallet_id"], cash_box.to_string());
    assert_eq!(rows[0]["wallet_name"], "Caja");

    // Money collected before the move stays where it was collected.
    assert_eq!(
        record(&pool, &cookie, cash_box, "adjustment_in", "10.00").await,
        StatusCode::CREATED
    );

    let (status, _) = call(
        &pool,
        &cookie,
        "PATCH",
        &format!("/api/v1/payment-methods/{method}"),
        Some(json!({ "wallet_id": bank })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = call(&pool, &cookie, "GET", "/api/v1/payment-methods", None).await;
    assert_eq!(list[0]["wallet_id"], bank.to_string());
    assert_eq!(list[0]["wallet_name"], "Banco");

    // The already-recorded movement did not follow the method.
    assert_eq!(balance(&pool, &cookie, cash_box).await, "10.00");
    assert_eq!(balance(&pool, &cookie, bank).await, "0.00");
}

#[sqlx::test]
async fn a_duplicate_method_name_is_rejected_ignoring_case(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;
    create_method(&pool, &cookie, "Cash", wallet).await;

    let (status, body) = call(
        &pool,
        &cookie,
        "POST",
        "/api/v1/payment-methods",
        Some(json!({ "name": "cash", "wallet_id": wallet })),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "the name is already in use");
}

#[sqlx::test]
async fn a_deactivated_method_is_hidden_unless_requested(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;
    let method = create_method(&pool, &cookie, "Cash", wallet).await;

    let (status, _) = call(
        &pool,
        &cookie,
        "PATCH",
        &format!("/api/v1/payment-methods/{method}"),
        Some(json!({ "is_active": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, active) = call(&pool, &cookie, "GET", "/api/v1/payment-methods", None).await;
    assert_eq!(active.as_array().unwrap().len(), 0);

    let (_, all) = call(
        &pool,
        &cookie,
        "GET",
        "/api/v1/payment-methods?include_inactive=true",
        None,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["is_active"], false);
}

// --- The ledger is history ----------------------------------------------

/// Enforced by the database, not by the absence of an endpoint: a mistake is
/// fixed with an adjustment, never by editing the line.
#[sqlx::test]
async fn wallet_movements_cannot_be_rewritten(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;
    assert_eq!(
        record(&pool, &cookie, wallet, "expense", "10.00").await,
        StatusCode::CREATED
    );

    let update = sqlx::query("UPDATE wallet_movements SET note = 'edited'")
        .execute(&pool)
        .await;
    assert!(update.is_err(), "UPDATE must be rejected by the trigger");

    let delete = sqlx::query("DELETE FROM wallet_movements")
        .execute(&pool)
        .await;
    assert!(delete.is_err(), "DELETE must be rejected by the trigger");

    assert_eq!(movement_count(&pool, wallet).await, 1);
}

/// Asking for 10.005 and storing 10.00 changes the request without saying so.
#[sqlx::test]
async fn an_amount_with_more_than_two_decimals_is_rejected(pool: PgPool) {
    let cookie = login(&pool).await;
    let wallet = create_wallet(&pool, &cookie, "Caja").await;

    let (status, body) = call(
        &pool,
        &cookie,
        "POST",
        &format!("/api/v1/wallets/{wallet}/movements"),
        Some(json!({ "kind": "expense", "amount": "10.005" })),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "the amount has more than 2 decimal places");
    assert_eq!(movement_count(&pool, wallet).await, 0);
    assert_eq!(balance(&pool, &cookie, wallet).await, "0.00");
}

/// Every kind must be counted with its own sign by the balance query. The match
/// below has no wildcard: adding a kind to the enum breaks this test at compile
/// time, which is the only way to notice that the SQL `CASE` needs it too.
#[sqlx::test]
async fn every_kind_is_counted_with_its_own_sign(pool: PgPool) {
    fn all_kinds() -> [WalletMovementKind; 5] {
        let kinds = [
            WalletMovementKind::SalePayment,
            WalletMovementKind::SaleRefund,
            WalletMovementKind::Expense,
            WalletMovementKind::AdjustmentIn,
            WalletMovementKind::AdjustmentOut,
        ];
        for kind in kinds {
            match kind {
                WalletMovementKind::SalePayment
                | WalletMovementKind::SaleRefund
                | WalletMovementKind::Expense
                | WalletMovementKind::AdjustmentIn
                | WalletMovementKind::AdjustmentOut => {}
            }
        }
        kinds
    }

    let _cookie = login(&pool).await;
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A sale payment needs a sale and a method to point at; the CHECK constraint
    // refuses an orphan, which is exactly what it is for.
    let sale_wallet = ledger::create_wallet(&pool, "Sale wallet").await.unwrap();
    let method = ledger::create_method(
        &pool,
        ledger::NewMethod {
            name: "Cash",
            wallet_id: sale_wallet,
        },
    )
    .await
    .unwrap();
    let sale = Uuid::now_v7();
    sqlx::query("INSERT INTO sales (id, status, created_by) VALUES ($1, 'draft', $2)")
        .bind(sale)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

    for (index, kind) in all_kinds().into_iter().enumerate() {
        // The ledger's linkage, spelled out once per kind. A sale payment and a
        // sale refund name the sale and the method they belong to; every manual
        // kind names neither. Written as a flat match, not nested branches, so
        // that which kinds carry the link is read directly instead of being
        // reconstructed from the order of an `if`.
        let (wallet, sale_id, payment_method_id) = match kind {
            WalletMovementKind::SalePayment => (sale_wallet, Some(sale), Some(method)),
            WalletMovementKind::SaleRefund => (
                ledger::create_wallet(&pool, &format!("Wallet {index}"))
                    .await
                    .unwrap(),
                Some(sale),
                Some(method),
            ),
            WalletMovementKind::Expense
            | WalletMovementKind::AdjustmentIn
            | WalletMovementKind::AdjustmentOut => (
                ledger::create_wallet(&pool, &format!("Wallet {index}"))
                    .await
                    .unwrap(),
                None,
                None,
            ),
        };

        ledger::insert_movement(
            &pool,
            NewWalletMovement {
                wallet_id: wallet,
                kind,
                amount: dec!(10.00),
                sale_id,
                payment_method_id,
                note: None,
                created_by: Some(owner),
            },
        )
        .await
        .unwrap();

        let balance = ledger::get_wallet(&pool, wallet).await.unwrap().balance;
        assert_eq!(
            balance,
            kind.signed_effect(dec!(10.00)),
            "{kind} must move the balance by its own sign"
        );
    }
}
