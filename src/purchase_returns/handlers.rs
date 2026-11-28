//! Purchase-return HTTP handlers.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{AppJson, AppPath};
use crate::purchase_returns::model::{PurchaseReturnDetail, ReturnLine, SurplusMode};
use crate::purchase_returns::service::{self, CreatePurchaseReturnInput, PurchaseReturnsError};
use crate::purchases::model::PurchaseStatus;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/purchases/{id}/returns",
        get(purchase_returns).post(create_purchase_return),
    )
}

// --- DTOs ---------------------------------------------------------------

/// One line to return to the supplier. Unit cost is never sent by the client:
/// the server uses the frozen cost from the linked purchase line.
#[derive(Deserialize, ToSchema)]
pub struct PurchaseReturnLineRequest {
    pub product_id: Uuid,
    #[schema(value_type = String, example = "2")]
    pub quantity: Decimal,
}

/// Request body for `POST /api/v1/purchases/{id}/returns`.
#[derive(Deserialize, ToSchema)]
pub struct CreatePurchaseReturnRequest {
    /// Why the goods are being returned. Required and stored on the document.
    pub reason: String,
    pub items: Vec<PurchaseReturnLineRequest>,
    /// How any already-paid surplus above the purchase's open payable should
    /// be handled (`wallet_refund` or `supplier_credit`). Defaults to
    /// `wallet_refund`.
    #[serde(default)]
    pub surplus_mode: SurplusMode,
    /// Optional retry key unique per purchase. Repeating a request with the
    /// same key returns the original return with `200` instead of writing a
    /// duplicate.
    #[schema(max_length = 100)]
    pub idempotency_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct PurchaseReturnItemResponse {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: String,
    pub unit_cost: String,
    pub subtotal: String,
}

/// One wallet inflow written when `surplus_mode = wallet_refund`.
#[derive(Serialize, ToSchema)]
pub struct PurchaseReturnRefundResponse {
    pub wallet_id: Uuid,
    pub wallet_name: String,
    pub payment_method_id: Uuid,
    pub payment_method_name: String,
    pub amount: String,
}

#[derive(Serialize, ToSchema)]
pub struct PurchaseReturnResponse {
    pub id: Uuid,
    pub purchase_id: Uuid,
    pub surplus_mode: SurplusMode,
    pub reason: String,
    /// Server-calculated total from the returned quantities and frozen unit costs.
    pub total: String,
    /// Portion of `total` that cancelled open payable debt on the purchase.
    pub debt_cancelled: String,
    /// Portion of `total` left as unapplied supplier credit (`surplus_mode = supplier_credit`).
    pub credit_granted: String,
    pub items: Vec<PurchaseReturnItemResponse>,
    pub refunds: Vec<PurchaseReturnRefundResponse>,
    pub created_at: DateTime<Utc>,
}

/// One purchased line with cumulative returned and remaining returnable quantities.
#[derive(Serialize, ToSchema)]
pub struct ReturnablePurchaseLineResponse {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: String,
    pub unit_cost: String,
    pub returned_quantity: String,
    pub returnable_quantity: String,
}

/// Returnable state and issued returns for one purchase.
#[derive(Serialize, ToSchema)]
pub struct PurchaseReturnsSummaryResponse {
    pub purchase_id: Uuid,
    pub purchase_status: PurchaseStatus,
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    pub purchase_total: String,
    pub returned_total: String,
    pub returnable_total: String,
    /// Current open payable on this purchase (cancelled first by any new return).
    pub purchase_outstanding: String,
    /// Maximum paid surplus that can still be refunded to wallets for this purchase.
    pub wallet_refundable_capacity: String,
    pub items: Vec<ReturnablePurchaseLineResponse>,
    pub returns: Vec<PurchaseReturnResponse>,
}

// --- Handlers -----------------------------------------------------------

/// Return part or all of a completed purchase to its supplier.
///
/// Deducts returned goods from stock (rejecting with 409 when stock is
/// insufficient), cancels open payable debt on the purchase first, and settles
/// any already-paid surplus according to `surplus_mode`: either refunding into
/// the wallets that paid for the purchase (`wallet_refund`) or leaving it as
/// unapplied supplier credit (`supplier_credit`).
#[utoipa::path(
    post,
    operation_id = "create_purchase_return_doc",
    path = "/api/v1/purchases/{id}/returns",
    tag = "purchase_returns",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    request_body = CreatePurchaseReturnRequest,
    responses(
        (status = 201, description = "Purchase return issued", body = PurchaseReturnResponse),
        (status = 200, description = "Existing purchase return returned for a repeated idempotency key",
         body = PurchaseReturnResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Purchase not completed, quantity or total exceeds purchase, insufficient stock, insufficient wallet refund capacity, inactive supplier for credit, or busy supplier account",
         body = crate::openapi::Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid reason, idempotency key, product not in purchase, duplicate product, or invalid quantity",
         body = crate::openapi::Problem, content_type = "application/problem+json"),
    )
)]
async fn create_purchase_return(
    State(state): State<AppState>,
    user: AuthUser,
    AppPath(purchase_id): AppPath<Uuid>,
    AppJson(body): AppJson<CreatePurchaseReturnRequest>,
) -> Result<(StatusCode, Json<PurchaseReturnResponse>), AppError> {
    let lines: Vec<ReturnLine> = body
        .items
        .into_iter()
        .map(|item| ReturnLine {
            product_id: item.product_id,
            quantity: item.quantity,
        })
        .collect();

    let created = service::create(
        &state.pool,
        CreatePurchaseReturnInput {
            purchase_id,
            actor: user.id(),
            reason: &body.reason,
            lines: &lines,
            surplus_mode: body.surplus_mode,
            idempotency_key: body.idempotency_key.as_deref(),
        },
    )
    .await?;

    let status = if created.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(to_response(created.detail))))
}

/// Returnable lines, open payable, wallet refund capacity, and issued returns
/// for one purchase.
#[utoipa::path(
    get,
    operation_id = "get_purchase_returns_doc",
    path = "/api/v1/purchases/{id}/returns",
    tag = "purchase_returns",
    params(("id" = Uuid, Path, description = "Purchase ID")),
    responses(
        (status = 200, description = "Return summary for the purchase", body = PurchaseReturnsSummaryResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Purchase not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
async fn purchase_returns(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(purchase_id): AppPath<Uuid>,
) -> Result<Json<PurchaseReturnsSummaryResponse>, AppError> {
    let summary = service::for_purchase(&state.pool, purchase_id).await?;

    Ok(Json(PurchaseReturnsSummaryResponse {
        purchase_id: summary.purchase_id,
        purchase_status: summary.purchase_status,
        supplier_id: summary.supplier_id,
        supplier_name: summary.supplier_name,
        purchase_total: format_money(summary.purchase_total),
        returned_total: format_money(summary.returned_total),
        returnable_total: format_money(summary.returnable_total),
        purchase_outstanding: format_money(summary.purchase_outstanding),
        wallet_refundable_capacity: format_money(summary.wallet_refundable_capacity),
        items: summary
            .lines
            .into_iter()
            .map(|line| {
                let returnable = format_quantity(line.returnable_quantity());
                ReturnablePurchaseLineResponse {
                    product_id: line.product_id,
                    product_name: line.product_name,
                    quantity: format_quantity(line.quantity),
                    unit_cost: format_money(line.unit_cost),
                    returned_quantity: format_quantity(line.returned_quantity),
                    returnable_quantity: returnable,
                }
            })
            .collect(),
        returns: summary.returns.into_iter().map(to_response).collect(),
    }))
}

// --- Mapping ------------------------------------------------------------

use crate::money::{format_money, format_quantity};

fn to_response(detail: PurchaseReturnDetail) -> PurchaseReturnResponse {
    PurchaseReturnResponse {
        id: detail.ret.id,
        purchase_id: detail.ret.purchase_id,
        surplus_mode: detail.ret.surplus_mode,
        reason: detail.ret.reason,
        total: format_money(detail.ret.total),
        debt_cancelled: format_money(detail.debt_cancelled),
        credit_granted: format_money(detail.credit_granted),
        items: detail
            .items
            .into_iter()
            .map(|item| {
                let subtotal = format_money(item.subtotal());
                PurchaseReturnItemResponse {
                    product_id: item.product_id,
                    product_name: item.product_name,
                    quantity: format_quantity(item.quantity),
                    unit_cost: format_money(item.unit_cost),
                    subtotal,
                }
            })
            .collect(),
        refunds: detail
            .refunds
            .into_iter()
            .map(|refund| PurchaseReturnRefundResponse {
                wallet_id: refund.wallet_id,
                wallet_name: refund.wallet_name,
                payment_method_id: refund.payment_method_id,
                payment_method_name: refund.payment_method_name,
                amount: format_money(refund.amount),
            })
            .collect(),
        created_at: detail.ret.created_at,
    }
}

impl From<PurchaseReturnsError> for AppError {
    fn from(err: PurchaseReturnsError) -> Self {
        match err {
            PurchaseReturnsError::PurchaseNotFound => AppError::NotFound,
            PurchaseReturnsError::PurchaseNotReturnable
            | PurchaseReturnsError::ReturnExceedsPurchase { .. }
            | PurchaseReturnsError::InsufficientWalletRefundCapacity { .. }
            | PurchaseReturnsError::SupplierInactive
            | PurchaseReturnsError::SupplierBusy => AppError::Conflict(err.to_string()),
            PurchaseReturnsError::ExceedsReturnable {
                product_name,
                available,
                requested,
            } => AppError::Conflict(format!(
                "cannot return {} of {product_name}: only {} left to return",
                format_quantity(requested),
                format_quantity(available)
            )),
            PurchaseReturnsError::InsufficientStock {
                product_name,
                available,
                requested,
            } => AppError::Conflict(format!(
                "insufficient stock for {product_name}: available {}, requested {}",
                format_quantity(available),
                format_quantity(requested)
            )),
            PurchaseReturnsError::Empty
            | PurchaseReturnsError::QuantityNotPositive
            | PurchaseReturnsError::QuantityPrecision
            | PurchaseReturnsError::QuantityTooLarge
            | PurchaseReturnsError::TotalTooLarge
            | PurchaseReturnsError::DuplicateProduct
            | PurchaseReturnsError::ReasonMissing
            | PurchaseReturnsError::ReasonTooLong
            | PurchaseReturnsError::IdempotencyKeyMissing
            | PurchaseReturnsError::IdempotencyKeyTooLong
            | PurchaseReturnsError::ProductNotInPurchase => AppError::Validation(err.to_string()),
            PurchaseReturnsError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}
