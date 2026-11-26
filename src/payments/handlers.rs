//! Payments HTTP handlers.
//!
//! Authorization: wallets, payment methods and manual movements are operational
//! work, so every route requires a session and nothing else — the rule the
//! catalog already follows. A cashier must be able to charge with any method the
//! business configured.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{AppJson, AppPath, AppQuery};
use crate::payments::model::{
    PaymentMethod, WalletMovement, WalletMovementKind, WalletWithBalance,
};
use crate::payments::service::{
    self, CreateMethodInput, CreateWalletInput, ManualMovementInput, PaymentsError,
    UpdateMethodInput, UpdateWalletInput,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/wallets", get(list_wallets).post(create_wallet))
        .route("/wallets/{id}", get(get_wallet).patch(update_wallet))
        .route(
            "/wallets/{id}/movements",
            get(list_wallet_movements).post(record_manual_movement),
        )
        .route(
            "/payment-methods",
            get(list_payment_methods).post(create_payment_method),
        )
        .route("/payment-methods/{id}", patch(update_payment_method))
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct WalletResponse {
    pub id: Uuid,
    pub name: String,
    pub is_active: bool,
    /// Calculated: the signed sum of the wallet's movements, as a string with
    /// two decimals. `"-1500.00"` is a valid answer — expenses can exceed what
    /// the wallet holds.
    pub balance: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct PaymentMethodResponse {
    pub id: Uuid,
    pub name: String,
    /// The wallet this method pays into. The UI shows its name so nobody
    /// charges into the wrong account without noticing.
    pub wallet_id: Uuid,
    pub wallet_name: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct WalletMovementResponse {
    pub id: Uuid,
    pub kind: WalletMovementKind,
    /// Always positive; the kind carries the sign.
    pub amount: String,
    /// The wallet's effect of this movement, so the client does not replicate
    /// the sign rule.
    pub signed_amount: String,
    /// Set only for a `sale_payment` or `sale_refund`: which sale moved this money.
    pub sale_id: Option<Uuid>,
    /// Set only for a `purchase_payment`: which purchase paid out this money.
    pub purchase_id: Option<Uuid>,
    /// Set for `sale_payment`, `sale_refund`, `account_payment`, `account_payment_refund`,
    /// `purchase_payment`, `supplier_account_payment`, or `supplier_account_refund`:
    /// which payment method was used.
    pub payment_method_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct IdResponse {
    pub id: Uuid,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    /// Include deactivated rows. The settings screen needs them so a wallet can
    /// be brought back; the POS does not.
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListMovementsQuery {
    pub limit: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateWalletRequest {
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateWalletRequest {
    pub name: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreatePaymentMethodRequest {
    pub name: String,
    pub wallet_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdatePaymentMethodRequest {
    pub name: Option<String>,
    /// Moving a method to another wallet changes where **future** money lands.
    /// Nothing already recorded moves: every movement keeps the wallet it hit.
    pub wallet_id: Option<Uuid>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct ManualMovementRequest {
    /// `expense`, `adjustment_in` or `adjustment_out`. `sale_payment`,
    /// `sale_refund`, `account_payment`, `account_payment_refund` and
    /// `purchase_payment` are rejected: that money is recorded by charging or
    /// returning a sale, collecting or refunding a customer's account, or
    /// completing a purchase, not by hand.
    pub kind: WalletMovementKind,
    #[schema(value_type = String, example = "1200.00")]
    pub amount: Decimal,
    pub note: Option<String>,
}

// --- Handlers -----------------------------------------------------------

async fn list_wallets(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListQuery>,
) -> Result<Json<Vec<WalletResponse>>, AppError> {
    let wallets = service::list_wallets(&state.pool, query.include_inactive).await?;
    Ok(Json(wallets.into_iter().map(to_wallet_response).collect()))
}

async fn create_wallet(
    State(state): State<AppState>,
    _user: AuthUser,
    AppJson(body): AppJson<CreateWalletRequest>,
) -> Result<(StatusCode, Json<IdResponse>), AppError> {
    let id = service::create_wallet(&state.pool, CreateWalletInput { name: body.name }).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn get_wallet(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<Json<WalletResponse>, AppError> {
    let wallet = service::get_wallet(&state.pool, id).await?;
    Ok(Json(to_wallet_response(wallet)))
}

async fn update_wallet(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<UpdateWalletRequest>,
) -> Result<StatusCode, AppError> {
    service::update_wallet(
        &state.pool,
        id,
        UpdateWalletInput {
            name: body.name,
            is_active: body.is_active,
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_wallet_movements(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListMovementsQuery>,
) -> Result<Json<Vec<WalletMovementResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let rows = service::list_movements(&state.pool, id, limit).await?;
    Ok(Json(rows.into_iter().map(to_movement_response).collect()))
}

async fn record_manual_movement(
    State(state): State<AppState>,
    user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<ManualMovementRequest>,
) -> Result<(StatusCode, Json<IdResponse>), AppError> {
    let id = service::record_manual_movement(
        &state.pool,
        id,
        ManualMovementInput {
            kind: body.kind,
            amount: body.amount,
            note: body.note,
            actor_id: user.id(),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn list_payment_methods(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListQuery>,
) -> Result<Json<Vec<PaymentMethodResponse>>, AppError> {
    let methods = service::list_methods(&state.pool, query.include_inactive).await?;
    Ok(Json(methods.into_iter().map(to_method_response).collect()))
}

async fn create_payment_method(
    State(state): State<AppState>,
    _user: AuthUser,
    AppJson(body): AppJson<CreatePaymentMethodRequest>,
) -> Result<(StatusCode, Json<IdResponse>), AppError> {
    let id = service::create_method(
        &state.pool,
        CreateMethodInput {
            name: body.name,
            wallet_id: body.wallet_id,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn update_payment_method(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<UpdatePaymentMethodRequest>,
) -> Result<StatusCode, AppError> {
    service::update_method(
        &state.pool,
        id,
        UpdateMethodInput {
            name: body.name,
            wallet_id: body.wallet_id,
            is_active: body.is_active,
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Conversions --------------------------------------------------------

use crate::money::format_money;

fn to_wallet_response(item: WalletWithBalance) -> WalletResponse {
    WalletResponse {
        id: item.wallet.id,
        name: item.wallet.name,
        is_active: item.wallet.is_active,
        balance: format_money(item.balance),
        created_at: item.wallet.created_at,
        updated_at: item.wallet.updated_at,
    }
}

fn to_method_response(method: PaymentMethod) -> PaymentMethodResponse {
    PaymentMethodResponse {
        id: method.id,
        name: method.name,
        wallet_id: method.wallet_id,
        wallet_name: method.wallet_name,
        is_active: method.is_active,
        created_at: method.created_at,
        updated_at: method.updated_at,
    }
}

fn to_movement_response(row: WalletMovement) -> WalletMovementResponse {
    WalletMovementResponse {
        id: row.id,
        kind: row.kind,
        amount: format_money(row.amount),
        signed_amount: format_money(row.signed_amount()),
        sale_id: row.sale_id,
        purchase_id: row.purchase_id,
        payment_method_id: row.payment_method_id,
        note: row.note,
        created_by: row.created_by,
        created_at: row.created_at,
    }
}

// --- Errors -------------------------------------------------------------

impl From<PaymentsError> for AppError {
    fn from(err: PaymentsError) -> Self {
        match err {
            PaymentsError::Validation(detail) => AppError::Validation(detail),
            PaymentsError::WalletNotFound | PaymentsError::MethodNotFound => AppError::NotFound,
            PaymentsError::NameTaken => {
                AppError::Conflict("the name is already in use".to_string())
            }
            PaymentsError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------
//
// Stubs exist only for `#[utoipa::path]` to generate the PathItem; the real
// route is served by the handler above.

/// List wallets with their calculated balance.
#[utoipa::path(
    get,
    path = "/api/v1/wallets",
    tag = "payments",
    params(ListQuery),
    responses(
        (status = 200, description = "Wallets with balance", body = Vec<WalletResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_wallets_doc() {}

/// Create a wallet (a cash box, a bank account).
#[utoipa::path(
    post,
    path = "/api/v1/wallets",
    tag = "payments",
    request_body = CreateWalletRequest,
    responses(
        (status = 201, description = "Wallet created", body = IdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Name is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_wallet_doc() {}

/// One wallet with its calculated balance.
#[utoipa::path(
    get,
    path = "/api/v1/wallets/{id}",
    tag = "payments",
    params(("id" = Uuid, Path, description = "Wallet id")),
    responses(
        (status = 200, description = "Wallet with balance", body = WalletResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_wallet_doc() {}

/// Rename or (de)activate a wallet. Wallets are never deleted.
#[utoipa::path(
    patch,
    path = "/api/v1/wallets/{id}",
    tag = "payments",
    params(("id" = Uuid, Path, description = "Wallet id")),
    request_body = UpdateWalletRequest,
    responses(
        (status = 204, description = "Wallet updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Name is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn update_wallet_doc() {}

/// Movements of one wallet, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/wallets/{id}/movements",
    tag = "payments",
    params(("id" = Uuid, Path, description = "Wallet id"), ListMovementsQuery),
    responses(
        (status = 200, description = "Movements", body = Vec<WalletMovementResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_wallet_movements_doc() {}

/// Record money leaving or entering a wallet by hand.
///
/// `sale_payment`, `sale_refund`, `account_payment` and
/// `account_payment_refund` are rejected with 422: money that belongs to a
/// sale or a customer's account enters or leaves the ledger only through the
/// flow that also writes its counterpart, never by hand.
#[utoipa::path(
    post,
    path = "/api/v1/wallets/{id}/movements",
    tag = "payments",
    params(("id" = Uuid, Path, description = "Wallet id")),
    request_body = ManualMovementRequest,
    responses(
        (status = 201, description = "Movement recorded", body = IdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn record_manual_movement_doc() {}

/// List payment methods with the wallet each one pays into.
#[utoipa::path(
    get,
    path = "/api/v1/payment-methods",
    tag = "payments",
    params(ListQuery),
    responses(
        (status = 200, description = "Payment methods", body = Vec<PaymentMethodResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_payment_methods_doc() {}

/// Create a payment method pointing at a wallet.
///
/// This is the endpoint that makes a new method a data change instead of a
/// deploy.
#[utoipa::path(
    post,
    path = "/api/v1/payment-methods",
    tag = "payments",
    request_body = CreatePaymentMethodRequest,
    responses(
        (status = 201, description = "Payment method created", body = IdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Name is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_payment_method_doc() {}

/// Rename, re-point or (de)activate a payment method.
///
/// Re-pointing changes where future money lands. Movements already recorded keep
/// the wallet they hit: the ledger is history, not a live join.
#[utoipa::path(
    patch,
    path = "/api/v1/payment-methods/{id}",
    tag = "payments",
    params(("id" = Uuid, Path, description = "Payment method id")),
    request_body = UpdatePaymentMethodRequest,
    responses(
        (status = 204, description = "Payment method updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Payment method or wallet not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Name is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn update_payment_method_doc() {}
