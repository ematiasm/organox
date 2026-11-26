//! Purchases HTTP handlers.
//!
//! Mirrors `sales/handlers.rs`: the draft lives on the server, each item
//! mutation returns the recalculated full purchase, and completion writes
//! stock inflows and wallet outflows atomically.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::purchases::model::{Purchase, PurchasePaymentInput, PurchaseStatus};
use crate::purchases::service::{self, PurchaseError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/purchases/draft", post(open_draft))
        .route("/purchases", get(list_purchases))
        .route("/purchases/{id}", get(get_purchase))
        .route("/purchases/{id}/supplier", put(set_purchase_supplier))
        .route("/purchases/{id}/items", post(add_item))
        .route(
            "/purchases/{id}/items/{product_id}",
            put(set_item).delete(remove_item),
        )
        .route("/purchases/{id}/complete", post(complete_purchase))
        .route("/purchases/{id}/cancel", post(cancel_purchase))
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct PurchaseItemResponse {
    pub product_id: Uuid,
    pub product_name: String,
    #[schema(value_type = String, example = "10")]
    pub quantity: String,
    #[schema(value_type = String, example = "125.50")]
    pub unit_cost: String,
    #[schema(value_type = String, example = "1255.00")]
    pub subtotal: String,
}

#[derive(Serialize, ToSchema)]
pub struct PurchaseResponse {
    pub id: Uuid,
    pub status: PurchaseStatus,
    pub items: Vec<PurchaseItemResponse>,
    #[schema(value_type = String, example = "1255.00")]
    pub total: String,
    pub item_count: usize,
    pub supplier_id: Option<Uuid>,
    /// Live registry name while draft; frozen snapshot once completed.
    pub supplier_name: Option<String>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct PurchaseListResponse {
    pub items: Vec<PurchaseResponse>,
    pub limit: i64,
    pub offset: i64,
    pub total: i64,
}

#[derive(Serialize, ToSchema)]
pub struct PurchaseDraftResponse {
    pub id: Uuid,
    /// `true` if the operator's existing open draft was reused.
    pub reused: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CompletedPurchaseResponse {
    pub purchase_id: Uuid,
    #[schema(value_type = String, example = "1255.00")]
    pub total: String,
    pub movement_ids: Vec<Uuid>,
    pub payment_movement_ids: Vec<Uuid>,
    /// Unapplied supplier advance credit automatically allocated to this
    /// purchase upon completion (`"0.00"` when none applied).
    #[schema(value_type = String, example = "0.00")]
    pub credit_applied: String,
    pub completed_at: DateTime<Utc>,
}

/// Attaches a supplier to the draft, or clears it.
///
/// `supplier_id` must be present: clearing is an explicit `null`, and an
/// omitted field is rejected (`422`) instead of detaching the supplier silently.
#[derive(Deserialize, ToSchema)]
pub struct SetPurchaseSupplierRequest {
    /// Supplier to attach to the draft, or `null` to clear it.
    #[serde(default, deserialize_with = "present_option")]
    #[schema(value_type = Option<Uuid>)]
    pub supplier_id: Option<Option<Uuid>>,
}

/// Distinguishes an omitted field (`None`) from an explicit `null` (`Some(None)`).
fn present_option<'de, D>(deserializer: D) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, ToSchema)]
pub struct AddPurchaseItemRequest {
    pub product_id: Uuid,
    #[schema(value_type = String, example = "10")]
    pub quantity: Decimal,
    #[schema(value_type = String, example = "125.50")]
    pub unit_cost: Decimal,
}

#[derive(Deserialize, ToSchema)]
pub struct SetPurchaseItemRequest {
    #[schema(value_type = String, example = "10")]
    pub quantity: Decimal,
    #[schema(value_type = String, example = "125.50")]
    pub unit_cost: Decimal,
}

#[derive(Deserialize, ToSchema)]
pub struct PurchasePaymentRequest {
    pub payment_method_id: Uuid,
    #[schema(value_type = String, example = "1255.00")]
    pub amount: Decimal,
}

#[derive(Deserialize, ToSchema)]
pub struct CompletePurchaseRequest {
    #[serde(default)]
    pub payments: Vec<PurchasePaymentRequest>,
    /// Portion of the purchase total charged to the supplier's account instead
    /// of paid upfront from a wallet (defaults to `0.00`).
    #[serde(default)]
    #[schema(value_type = String, example = "0.00")]
    pub on_account: Decimal,
    pub note: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListPurchasesQuery {
    pub status: Option<PurchaseStatus>,
    pub supplier_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// --- Handlers -----------------------------------------------------------

async fn open_draft(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<(StatusCode, Json<PurchaseDraftResponse>), AppError> {
    let (id, reused) = service::open_draft(&state.pool, user.id()).await?;
    Ok((
        if reused {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(PurchaseDraftResponse { id, reused }),
    ))
}

async fn list_purchases(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(query): Query<ListPurchasesQuery>,
) -> Result<Json<PurchaseListResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);

    let (items, total) =
        service::list_purchases(&state.pool, query.status, query.supplier_id, limit, offset)
            .await?;

    Ok(Json(PurchaseListResponse {
        items: items.into_iter().map(to_response).collect(),
        limit,
        offset,
        total,
    }))
}

async fn get_purchase(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<PurchaseResponse>, AppError> {
    let purchase = service::get_purchase(&state.pool, id).await?;
    Ok(Json(to_response(purchase)))
}

async fn set_purchase_supplier(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetPurchaseSupplierRequest>,
) -> Result<StatusCode, AppError> {
    let supplier_id = body.supplier_id.ok_or_else(|| {
        AppError::Validation("the supplier_id field is required; pass null to clear it".into())
    })?;
    service::set_supplier(&state.pool, id, supplier_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_item(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AddPurchaseItemRequest>,
) -> Result<Json<PurchaseResponse>, AppError> {
    let purchase = service::add_item(
        &state.pool,
        id,
        body.product_id,
        body.quantity,
        body.unit_cost,
    )
    .await?;
    Ok(Json(to_response(purchase)))
}

async fn set_item(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((id, product_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetPurchaseItemRequest>,
) -> Result<Json<PurchaseResponse>, AppError> {
    let purchase =
        service::set_item(&state.pool, id, product_id, body.quantity, body.unit_cost).await?;
    Ok(Json(to_response(purchase)))
}

async fn remove_item(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((id, product_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PurchaseResponse>, AppError> {
    let purchase = service::remove_item(&state.pool, id, product_id).await?;
    Ok(Json(to_response(purchase)))
}

async fn complete_purchase(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CompletePurchaseRequest>,
) -> Result<Json<CompletedPurchaseResponse>, AppError> {
    let payments: Vec<PurchasePaymentInput> = body
        .payments
        .into_iter()
        .map(|p| PurchasePaymentInput {
            payment_method_id: p.payment_method_id,
            amount: p.amount,
        })
        .collect();

    let completed = service::complete(
        &state.pool,
        id,
        user.id(),
        &payments,
        body.on_account,
        body.note.as_deref(),
    )
    .await?;

    Ok(Json(CompletedPurchaseResponse {
        purchase_id: completed.purchase_id,
        total: format_money(completed.total),
        movement_ids: completed.movement_ids,
        payment_movement_ids: completed.payment_movement_ids,
        credit_applied: format_money(completed.credit_applied),
        completed_at: completed.completed_at,
    }))
}

async fn cancel_purchase(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    service::cancel(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Conversions --------------------------------------------------------

fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

fn to_response(purchase: Purchase) -> PurchaseResponse {
    let total = purchase.total();
    let item_count = purchase.item_count();
    PurchaseResponse {
        id: purchase.id,
        status: purchase.status,
        items: purchase
            .items
            .into_iter()
            .map(|item| PurchaseItemResponse {
                subtotal: format_money(item.subtotal()),
                product_id: item.product_id,
                product_name: item.product_name,
                quantity: item.quantity.normalize().to_string(),
                unit_cost: format_money(item.unit_cost),
            })
            .collect(),
        total: format_money(total),
        item_count,
        supplier_id: purchase.supplier_id,
        supplier_name: purchase.supplier_name,
        note: purchase.note,
        created_at: purchase.created_at,
        completed_at: purchase.completed_at,
    }
}

// --- Errors -------------------------------------------------------------

impl From<PurchaseError> for AppError {
    fn from(err: PurchaseError) -> Self {
        match err {
            PurchaseError::NotFound
            | PurchaseError::ProductNotFound
            | PurchaseError::SupplierNotFound => AppError::NotFound,
            PurchaseError::ProductInactive
            | PurchaseError::ServiceNotPurchasable
            | PurchaseError::NotEditable
            | PurchaseError::SupplierInactive
            | PurchaseError::SupplierAccountBusy => AppError::Conflict(err.to_string()),
            PurchaseError::OnAccountExceedsCreditLimit { available } => {
                AppError::Conflict(format!(
                    "the on-account amount exceeds the {} the supplier still has available",
                    format_money(available)
                ))
            }
            PurchaseError::PaymentMismatch {
                total,
                paid,
                on_account,
            } => {
                let detail = if on_account > Decimal::ZERO {
                    format!(
                        "the payments and the on-account amount add up to {}, but the purchase total is {}",
                        format_money(paid),
                        format_money(total)
                    )
                } else {
                    format!(
                        "the payments ({}) do not add up to the purchase total ({})",
                        format_money(paid),
                        format_money(total)
                    )
                };
                AppError::Validation(detail)
            }
            PurchaseError::EmptyPurchase
            | PurchaseError::SupplierRequired
            | PurchaseError::QuantityNotPositive
            | PurchaseError::QuantityPrecision
            | PurchaseError::QuantityTooLarge
            | PurchaseError::UnitCostNegative
            | PurchaseError::UnitCostPrecision
            | PurchaseError::UnitCostTooLarge
            | PurchaseError::TotalTooLarge
            | PurchaseError::PaymentAmountNotPositive
            | PurchaseError::PaymentAmountPrecision
            | PurchaseError::PaymentAmountTooLarge
            | PurchaseError::PaymentMethodUnavailable
            | PurchaseError::OnAccountNegative
            | PurchaseError::OnAccountPrecision
            | PurchaseError::OnAccountTooLarge => AppError::Validation(err.to_string()),
            PurchaseError::SupplierLookup(inner) => AppError::Internal(inner.to_string()),
            PurchaseError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------

/// Open or reuse the current operator's draft purchase.
#[utoipa::path(
    post,
    path = "/api/v1/purchases/draft",
    tag = "purchases",
    responses(
        (status = 200, description = "Existing open draft reused", body = PurchaseDraftResponse),
        (status = 201, description = "New draft created", body = PurchaseDraftResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn open_draft_doc() {}

/// List purchases newest first with optional status and supplier filters.
#[utoipa::path(
    get,
    path = "/api/v1/purchases",
    tag = "purchases",
    params(ListPurchasesQuery),
    responses(
        (status = 200, description = "Purchase page", body = PurchaseListResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_purchases_doc() {}

/// Purchase details including its items and calculated or frozen total.
#[utoipa::path(
    get,
    path = "/api/v1/purchases/{id}",
    tag = "purchases",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    responses(
        (status = 200, description = "Purchase", body = PurchaseResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_purchase_doc() {}

/// Attach or clear the supplier on a draft purchase.
#[utoipa::path(
    put,
    path = "/api/v1/purchases/{id}/supplier",
    tag = "purchases",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    request_body = SetPurchaseSupplierRequest,
    responses(
        (status = 204, description = "Supplier updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase or supplier not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is closed or supplier is inactive", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "supplier_id field omitted", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn set_purchase_supplier_doc() {}

/// Add a product to a draft purchase (sums quantity and updates unit_cost if already present).
#[utoipa::path(
    post,
    path = "/api/v1/purchases/{id}/items",
    tag = "purchases",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    request_body = AddPurchaseItemRequest,
    responses(
        (status = 200, description = "Updated draft", body = PurchaseResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase or product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is closed, product is inactive, or product is a service", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid quantity or unit cost", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn add_item_doc() {}

/// Replace quantity and unit cost of an item in a draft purchase.
#[utoipa::path(
    put,
    path = "/api/v1/purchases/{id}/items/{product_id}",
    tag = "purchases",
    params(
        ("id" = Uuid, Path, description = "Purchase ID"),
        ("product_id" = Uuid, Path, description = "Product ID"),
    ),
    request_body = SetPurchaseItemRequest,
    responses(
        (status = 200, description = "Updated draft", body = PurchaseResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase or item not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid quantity or unit cost", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn set_item_doc() {}

/// Remove an item from a draft purchase.
#[utoipa::path(
    delete,
    path = "/api/v1/purchases/{id}/items/{product_id}",
    tag = "purchases",
    params(
        ("id" = Uuid, Path, description = "Purchase ID"),
        ("product_id" = Uuid, Path, description = "Product ID"),
    ),
    responses(
        (status = 200, description = "Updated draft", body = PurchaseResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase or item not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn remove_item_doc() {}

/// Complete a draft purchase: record stock inflows and cash wallet outflows atomically.
#[utoipa::path(
    post,
    path = "/api/v1/purchases/{id}/complete",
    tag = "purchases",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    request_body = CompletePurchaseRequest,
    responses(
        (status = 200, description = "Purchase completed", body = CompletedPurchaseResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase, supplier or product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is closed, supplier is inactive, or product is a service", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Missing supplier, empty purchase, or invalid payments", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn complete_purchase_doc() {}

/// Cancel a draft purchase. Completed purchases cannot be cancelled.
#[utoipa::path(
    post,
    path = "/api/v1/purchases/{id}/cancel",
    tag = "purchases",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    responses(
        (status = 204, description = "Purchase cancelled"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase is already closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn cancel_purchase_doc() {}
