//! Suppliers HTTP handlers.
//!
//! Authorization: every route requires a valid session. Registering and editing
//! suppliers is operational work, so both `owner` and `manager` may access it.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{AppJson, AppPath, AppQuery};
use crate::suppliers::model::Supplier;
use crate::suppliers::service::{self, CreateSupplierInput, SupplierError, UpdateSupplierInput};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/suppliers", get(list_suppliers).post(create_supplier))
        .route("/suppliers/{id}", get(get_supplier).patch(update_supplier))
        .route("/suppliers/{id}/deactivate", post(deactivate_supplier))
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct SupplierResponse {
    pub id: Uuid,
    pub name: String,
    /// `null` when the supplier was registered without a tax document.
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    /// `null` means unlimited, `"0.00"` means cash only.
    pub credit_limit: Option<String>,
    /// Current account balance derived from `supplier_account_entries`.
    /// Positive means we owe the supplier; negative means advance credit held.
    #[schema(value_type = String, example = "120.00")]
    pub balance: String,
    /// Money paid to the supplier not yet allocated to a purchase or refunded (`U`).
    #[schema(value_type = String, example = "0.00")]
    pub available_credit: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct SupplierListResponse {
    pub items: Vec<SupplierResponse>,
    /// Page size actually applied (defaults to 50, capped at 200).
    pub limit: i64,
    /// Offset of `items[0]` in the filtered set.
    pub offset: i64,
    /// Total matching suppliers across all pages.
    pub total: i64,
}

#[derive(Serialize, ToSchema)]
pub struct SupplierIdResponse {
    pub id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateSupplierRequest {
    pub name: String,
    /// Optional and unique when present. A blank string means "no document".
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// Optional credit limit: absent or `null` means unlimited, `0` means cash
    /// only. Never negative, at most two decimals.
    #[schema(value_type = Option<String>, example = "100.00")]
    pub credit_limit: Option<Decimal>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateSupplierRequest {
    pub name: Option<String>,
    /// `null` means "leave the document as it is", never "clear it".
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: Option<bool>,
    /// Tri-state: omitted leaves the limit untouched, explicit `null` clears
    /// it back to unlimited, and a value sets the cap.
    #[serde(default, deserialize_with = "present_decimal_option")]
    #[schema(value_type = Option<String>, example = "100.00")]
    pub credit_limit: Option<Option<Decimal>>,
}

fn present_decimal_option<'de, D>(deserializer: D) -> Result<Option<Option<Decimal>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Decimal>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSuppliersQuery {
    /// Include deactivated suppliers. Absent, only active suppliers are returned.
    #[serde(default)]
    pub include_inactive: bool,
    /// Matches name or document, case-insensitively.
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// --- Handlers -----------------------------------------------------------

async fn list_suppliers(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListSuppliersQuery>,
) -> Result<Json<SupplierListResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);

    let (items, total) = service::list_suppliers(
        &state.pool,
        query.include_inactive,
        query.search.as_deref(),
        limit,
        offset,
    )
    .await?;

    Ok(Json(SupplierListResponse {
        items: items.into_iter().map(to_response).collect(),
        limit,
        offset,
        total,
    }))
}

async fn create_supplier(
    State(state): State<AppState>,
    _user: AuthUser,
    AppJson(body): AppJson<CreateSupplierRequest>,
) -> Result<(StatusCode, Json<SupplierIdResponse>), AppError> {
    let id = service::create_supplier(
        &state.pool,
        CreateSupplierInput {
            name: body.name,
            tax_id: body.tax_id,
            email: body.email,
            phone: body.phone,
            credit_limit: body.credit_limit,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(SupplierIdResponse { id })))
}

async fn get_supplier(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<Json<SupplierResponse>, AppError> {
    let supplier = service::get_supplier(&state.pool, id).await?;
    Ok(Json(to_response(supplier)))
}

async fn update_supplier(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<UpdateSupplierRequest>,
) -> Result<StatusCode, AppError> {
    service::update_supplier(
        &state.pool,
        id,
        UpdateSupplierInput {
            name: body.name,
            tax_id: body.tax_id,
            email: body.email,
            phone: body.phone,
            is_active: body.is_active,
            credit_limit: body.credit_limit,
        },
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn deactivate_supplier(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<StatusCode, AppError> {
    service::deactivate_supplier(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Conversions --------------------------------------------------------

use crate::money::format_money;

fn to_response(supplier: Supplier) -> SupplierResponse {
    SupplierResponse {
        id: supplier.id,
        name: supplier.name,
        tax_id: supplier.tax_id,
        email: supplier.email,
        phone: supplier.phone,
        is_active: supplier.is_active,
        credit_limit: supplier.credit_limit.map(format_money),
        balance: format_money(supplier.balance),
        available_credit: format_money(supplier.available_credit),
        created_at: supplier.created_at,
        updated_at: supplier.updated_at,
    }
}

// --- Errors -------------------------------------------------------------

impl From<SupplierError> for AppError {
    fn from(err: SupplierError) -> Self {
        match err {
            SupplierError::Validation(validation) => AppError::Validation(validation.to_string()),
            SupplierError::TaxIdTaken => {
                AppError::Conflict("the document is already in use".to_string())
            }
            SupplierError::OpenAccountPosition { .. } | SupplierError::AccountBusy => {
                AppError::Conflict(err.to_string())
            }
            SupplierError::NotFound => AppError::NotFound,
            SupplierError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------

/// List suppliers alphabetically by name.
#[utoipa::path(
    get,
    path = "/api/v1/suppliers",
    tag = "suppliers",
    params(ListSuppliersQuery),
    responses(
        (status = 200, description = "Supplier page", body = SupplierListResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_suppliers_doc() {}

/// Register a supplier.
///
/// `tax_id` is optional and unique when present; a duplicate returns 409.
/// `credit_limit` is optional: absent or `null` means unlimited, `0` means cash only.
#[utoipa::path(
    post,
    path = "/api/v1/suppliers",
    tag = "suppliers",
    request_body = CreateSupplierRequest,
    responses(
        (status = 201, description = "Supplier created", body = SupplierIdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Document is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_supplier_doc() {}

/// Supplier details with derived account balance and available credit.
#[utoipa::path(
    get,
    path = "/api/v1/suppliers/{id}",
    tag = "suppliers",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    responses(
        (status = 200, description = "Supplier", body = SupplierResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_supplier_doc() {}

/// Update only the fields sent.
///
/// `credit_limit` is tri-state: omitted leaves it unchanged, explicit `null`
/// clears it back to unlimited, and a value sets the cap. Deactivating with
/// `is_active: false` requires a zero account position (otherwise 409).
#[utoipa::path(
    patch,
    path = "/api/v1/suppliers/{id}",
    tag = "suppliers",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    request_body = UpdateSupplierRequest,
    responses(
        (status = 204, description = "Updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Document in use, open account position, or account busy", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn update_supplier_doc() {}

/// Soft deactivation. Idempotent. Requires a zero account position (409 otherwise).
#[utoipa::path(
    post,
    path = "/api/v1/suppliers/{id}/deactivate",
    tag = "suppliers",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    responses(
        (status = 204, description = "Deactivated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Open account position or account busy", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn deactivate_supplier_doc() {}
