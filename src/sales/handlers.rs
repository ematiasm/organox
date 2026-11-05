//! Sales HTTP handlers.
//!
//! The cart lives on the server: each action returns the recalculated **full
//! sale**. The client neither adds nor validates anything—it only displays.
//! This is the difference between a chargeable total and one the browser believes.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
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
use crate::sales::model::{Sale, SaleStatus};
use crate::sales::service::{self, SalesError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sales/draft", post(open_draft))
        .route("/sales/{id}", get(get_sale))
        .route("/sales/{id}/items", post(add_item))
        .route(
            "/sales/{id}/items/{product_id}",
            put(set_quantity).delete(remove_item),
        )
        .route("/sales/{id}/complete", post(complete_sale))
        .route("/sales/{id}/cancel", post(cancel_sale))
        .route("/sales", get(list_sales))
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct SaleItemResponse {
    pub product_id: Uuid,
    pub product_name: String,
    /// Quantity as a string: `numeric(12,3)` does not fit in a float without
    /// losing precision for fractional quantities.
    pub quantity: String,
    pub unit_price: String,
    pub subtotal: String,
}

#[derive(Serialize, ToSchema)]
pub struct SaleResponse {
    pub id: Uuid,
    pub status: SaleStatus,
    pub items: Vec<SaleItemResponse>,
    /// Calculated on the server. The client displays it, not decides it.
    pub total: String,
    pub item_count: usize,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct DraftResponse {
    pub id: Uuid,
    /// `true` if the existing open draft was reused.
    pub reused: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CompletedSaleResponse {
    pub sale_id: Uuid,
    pub total: String,
    pub movement_ids: Vec<Uuid>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct AddItemRequest {
    pub product_id: Uuid,
    #[schema(value_type = String, example = "2")]
    pub quantity: Decimal,
}

#[derive(Deserialize, ToSchema)]
pub struct SetQuantityRequest {
    #[schema(value_type = String, example = "3")]
    pub quantity: Decimal,
}

// --- Handlers -----------------------------------------------------------

async fn open_draft(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<(StatusCode, Json<DraftResponse>), AppError> {
    let existing = crate::sales::repository::find_open_draft_for_user(&state.pool, user.id())
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let reused = existing.is_some();
    let id = service::open_draft(&state.pool, user.id()).await?;

    Ok((
        if reused {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(DraftResponse { id, reused }),
    ))
}

async fn get_sale(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::get(&state.pool, id).await?;
    Ok(Json(to_response(sale)))
}

async fn add_item(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AddItemRequest>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::add_item(&state.pool, id, body.product_id, body.quantity).await?;
    Ok(Json(to_response(sale)))
}

async fn set_quantity(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((id, product_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetQuantityRequest>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::set_item_quantity(&state.pool, id, product_id, body.quantity).await?;
    Ok(Json(to_response(sale)))
}

async fn remove_item(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((id, product_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::remove_item(&state.pool, id, product_id).await?;
    Ok(Json(to_response(sale)))
}

async fn complete_sale(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<CompletedSaleResponse>, AppError> {
    let done = service::complete(&state.pool, id, user.id()).await?;
    Ok(Json(CompletedSaleResponse {
        sale_id: done.sale_id,
        total: format_money(done.total),
        movement_ids: done.movement_ids,
        completed_at: done.completed_at,
    }))
}

async fn cancel_sale(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    service::cancel(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sales(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<SaleResponse>>, AppError> {
    let sales = service::list_completed(&state.pool, 50).await?;
    Ok(Json(sales.into_iter().map(to_response).collect()))
}

// --- Conversions --------------------------------------------------------

/// Money with exactly 2 decimals (`"1500.50"`), as in the catalog.
fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

/// Quantities without extra zeros (`"2"`, `"1.5"`).
fn format_quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

fn to_response(sale: Sale) -> SaleResponse {
    let items: Vec<SaleItemResponse> = sale
        .items
        .iter()
        .map(|i| SaleItemResponse {
            product_id: i.product_id,
            product_name: i.product_name.clone(),
            quantity: format_quantity(i.quantity),
            unit_price: format_money(i.unit_price),
            subtotal: format_money(i.subtotal()),
        })
        .collect();

    SaleResponse {
        id: sale.id,
        status: sale.status,
        item_count: items.len(),
        total: format_money(sale.total()),
        items,
        created_at: sale.created_at,
        completed_at: sale.completed_at,
    }
}

impl From<SalesError> for AppError {
    fn from(err: SalesError) -> Self {
        match err {
            SalesError::NotFound => AppError::NotFound,
            SalesError::ProductNotFound => AppError::NotFound,
            SalesError::ServiceNotSellable | SalesError::QuantityNotPositive => {
                AppError::Validation(err.to_string())
            }
            SalesError::InsufficientStock {
                product_name,
                available,
                requested,
            } => AppError::Conflict(format!(
                "insufficient stock for {product_name}: available {}, requested {}",
                format_quantity(available),
                format_quantity(requested)
            )),
            SalesError::NotEditable => AppError::Conflict(err.to_string()),
            SalesError::EmptySale => AppError::Validation(err.to_string()),
            SalesError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------
//
// Stubs exist only for `#[utoipa::path]` to generate the PathItem.

/// Open a sale draft, or return the one already open.
///
/// A user has **one** draft at a time: reloading the sale screen does not
/// create new carts.
#[utoipa::path(
    post,
    path = "/api/v1/sales/draft",
    tag = "sales",
    responses(
        (status = 201, description = "New draft", body = DraftResponse),
        (status = 200, description = "Open draft reused", body = DraftResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn open_draft_doc() {}

/// Sale with its items and server-calculated total.
#[utoipa::path(
    get,
    path = "/api/v1/sales/{id}",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    responses(
        (status = 200, description = "Sale", body = SaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_sale_doc() {}

/// Add a product to the draft. If already present, add the quantity.
///
/// Returns the recalculated sale: the client does not add anything.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/items",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    request_body = AddItemRequest,
    responses(
        (status = 200, description = "Sale updated", body = SaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale or product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Sale is already closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid quantity or item is a service", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn add_item_doc() {}

/// Set an item's quantity. Quantity 0 removes it.
#[utoipa::path(
    put,
    path = "/api/v1/sales/{id}/items/{product_id}",
    tag = "sales",
    params(
        ("id" = Uuid, Path, description = "Sale ID"),
        ("product_id" = Uuid, Path, description = "Product ID"),
    ),
    request_body = SetQuantityRequest,
    responses(
        (status = 200, description = "Sale updated", body = SaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale or product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Sale is already closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn set_quantity_doc() {}

/// Remove an item from the draft.
#[utoipa::path(
    delete,
    path = "/api/v1/sales/{id}/items/{product_id}",
    tag = "sales",
    params(
        ("id" = Uuid, Path, description = "Sale ID"),
        ("product_id" = Uuid, Path, description = "Product ID"),
    ),
    responses(
        (status = 200, description = "Sale updated", body = SaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale or product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn remove_item_doc() {}

/// Complete the sale.
///
/// In one transaction: validate stock with the product row locked, write an
/// outflow movement per item and close the sale. If stock is insufficient,
/// reject with 409 and leave **no** movements or status change.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/complete",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    responses(
        (status = 200, description = "Sale completed", body = CompletedSaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Insufficient stock or sale already closed", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Sale is empty", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn complete_sale_doc() {}

/// Cancel a draft. Does not touch the stock ledger, because it never did.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/cancel",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    responses(
        (status = 204, description = "Draft cancelled"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Sale is no longer editable", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn cancel_sale_doc() {}

/// Latest completed sales.
#[utoipa::path(
    get,
    path = "/api/v1/sales",
    tag = "sales",
    responses(
        (status = 200, description = "Completed sales", body = Vec<SaleResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_sales_doc() {}
