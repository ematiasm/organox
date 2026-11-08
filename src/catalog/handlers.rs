//! Catalog HTTP handlers.
//!
//! Authorization: the entire catalog API requires a session. Reading and writing
//! the catalog is operational work (`manager`), not user administration.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::catalog::model::{self, MovementReason, ProductKind};
use crate::catalog::repository::MovementRow;
use crate::catalog::service::{
    self, CatalogError, CreateProductInput, RecordMovementInput, UpdateProductInput,
};
use crate::catalog::validation::ValidationError;
use crate::error::AppError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/products", get(list_products).post(create_product))
        .route("/products/{id}", get(get_product).patch(update_product))
        .route("/products/{id}/deactivate", post(deactivate_product))
        .route(
            "/products/{id}/movements",
            get(list_movements).post(record_movement),
        )
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct ProductResponse {
    pub id: Uuid,
    pub kind: ProductKind,
    pub sku: Option<String>,
    pub name: String,
    pub description: Option<String>,
    /// String, not number: a JSON `float` loses precision for money and
    /// fractional quantities. The client parses it as a decimal.
    pub price: Option<String>,
    pub min_stock: Option<String>,
    /// `null` for services: not zero, but "not applicable".
    pub stock: Option<String>,
    /// `null` when not applicable (service) or no threshold is configured.
    pub low_stock: Option<bool>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct ProductListResponse {
    pub items: Vec<ProductResponse>,
    /// Page size actually applied (the request defaults to 50, is capped at 200).
    pub limit: i64,
    /// Offset of `items[0]` in the filtered set.
    pub offset: i64,
    /// How many products the filters match in total, ignoring the page. A client
    /// needs this to tell a full page from the last one.
    pub total: i64,
}

#[derive(Serialize, ToSchema)]
pub struct IdResponse {
    pub id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct MovementResponse {
    pub id: Uuid,
    pub reason: MovementReason,
    pub quantity: String,
    /// Signed quantity: what the client needs to show the stock effect without
    /// replicating the sign rule.
    pub signed_quantity: String,
    pub unit_cost: Option<String>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListProductsQuery {
    pub kind: Option<ProductKind>,
    #[serde(default)]
    pub include_inactive: bool,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateProductRequest {
    pub kind: ProductKind,
    pub sku: Option<String>,
    pub name: String,
    pub description: Option<String>,
    /// Documented as `string` although the server also accepts numbers:
    /// `rust_decimal` deserializes both, but the frontend contract—and the only
    /// one preserving precision—is the string. See `docs/api-contract.md`.
    #[schema(value_type = Option<String>, example = "1500.50")]
    pub price: Option<Decimal>,
    #[schema(value_type = Option<String>, example = "10")]
    pub min_stock: Option<Decimal>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateProductRequest {
    pub sku: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    #[schema(value_type = Option<String>, example = "1500.50")]
    pub price: Option<Decimal>,
    #[schema(value_type = Option<String>, example = "10")]
    pub min_stock: Option<Decimal>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct RecordMovementRequest {
    pub reason: MovementReason,
    #[schema(value_type = String, example = "12.5")]
    pub quantity: Decimal,
    #[schema(value_type = Option<String>, example = "1200.00")]
    pub unit_cost: Option<Decimal>,
    pub note: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListMovementsQuery {
    pub limit: Option<i64>,
}

// --- Handlers -----------------------------------------------------------

async fn list_products(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(query): Query<ListProductsQuery>,
) -> Result<Json<ProductListResponse>, AppError> {
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);

    let (items, total) = service::list_products(
        &state.pool,
        query.kind,
        query.include_inactive,
        query.search.as_deref(),
        limit,
        offset,
    )
    .await?;

    Ok(Json(ProductListResponse {
        items: items.into_iter().map(to_response).collect(),
        limit,
        offset,
        total,
    }))
}

async fn create_product(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(body): Json<CreateProductRequest>,
) -> Result<(StatusCode, Json<IdResponse>), AppError> {
    let id = service::create_product(
        &state.pool,
        CreateProductInput {
            kind: body.kind,
            sku: body.sku,
            name: body.name,
            description: body.description,
            price: body.price,
            min_stock: body.min_stock,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn get_product(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ProductResponse>, AppError> {
    let item = service::get_product(&state.pool, id).await?;
    Ok(Json(to_response(item)))
}

async fn update_product(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProductRequest>,
) -> Result<StatusCode, AppError> {
    service::update_product(
        &state.pool,
        id,
        UpdateProductInput {
            sku: body.sku,
            name: body.name,
            description: body.description,
            price: body.price,
            min_stock: body.min_stock,
            is_active: body.is_active,
        },
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn deactivate_product(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    service::deactivate_product(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn record_movement(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RecordMovementRequest>,
) -> Result<(StatusCode, Json<IdResponse>), AppError> {
    let id = service::record_movement(
        &state.pool,
        RecordMovementInput {
            product_id: id,
            reason: body.reason,
            quantity: body.quantity,
            unit_cost: body.unit_cost,
            note: body.note,
            actor_id: user.id(),
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn list_movements(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<Uuid>,
    Query(query): Query<ListMovementsQuery>,
) -> Result<Json<Vec<MovementResponse>>, AppError> {
    let rows = service::list_movements(&state.pool, id, query.limit.unwrap_or(50)).await?;
    Ok(Json(rows.into_iter().map(to_movement_response).collect()))
}

// --- Conversions --------------------------------------------------------

/// Money with exactly 2 decimals: `"1500.50"`, not `"1500.5"` or
/// `"1500.500"`. A price is always displayed with its cents.
fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

/// Quantity without extra zeros: `"12.5"`, not `"12.500"`. The column is
/// `numeric(12,3)` and `SUM` returns the full scale; displaying it that way
/// would leak a storage detail into the HTTP contract.
fn format_quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

fn to_response(item: model::ProductWithStock) -> ProductResponse {
    let low_stock = model::is_low_stock(&item.product, item.stock);

    ProductResponse {
        id: item.product.id,
        kind: item.product.kind,
        sku: item.product.sku,
        name: item.product.name,
        description: item.product.description,
        price: item.product.price.map(format_money),
        min_stock: item.product.min_stock.map(format_quantity),
        stock: item.stock.map(format_quantity),
        low_stock,
        is_active: item.product.is_active,
        created_at: item.product.created_at,
        updated_at: item.product.updated_at,
    }
}

fn to_movement_response(row: MovementRow) -> MovementResponse {
    MovementResponse {
        id: row.id,
        reason: row.reason,
        quantity: format_quantity(row.quantity),
        signed_quantity: format_quantity(row.signed_quantity),
        unit_cost: row.unit_cost.map(format_money),
        note: row.note,
        created_by: row.created_by,
        created_at: row.created_at,
    }
}

// --- Errors -------------------------------------------------------------

impl From<CatalogError> for AppError {
    fn from(err: CatalogError) -> Self {
        match err {
            CatalogError::Validation(v) => match v {
                ValidationError::ProductNotFound => AppError::NotFound,
                other => AppError::Validation(other.to_string()),
            },
            CatalogError::SkuTaken => AppError::Conflict("the SKU is already in use".to_string()),
            CatalogError::NotFound => AppError::NotFound,
            CatalogError::InsufficientStock {
                available,
                requested,
            } => AppError::Conflict(format!(
                "insufficient stock: available {available}, requested {requested}"
            )),
            CatalogError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------
//
// Stubs exist only for `#[utoipa::path]` to generate the PathItem. The real
// route is served by the handler above.

/// List the catalog with calculated stock.
#[utoipa::path(
    get,
    path = "/api/v1/products",
    tag = "catalog",
    params(ListProductsQuery),
    responses(
        (status = 200, description = "Product page", body = ProductListResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_products_doc() {}

/// Create a product or service.
///
/// A service does not track stock: `min_stock` is rejected with 422.
#[utoipa::path(
    post,
    path = "/api/v1/products",
    tag = "catalog",
    request_body = CreateProductRequest,
    responses(
        (status = 201, description = "Product created", body = IdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "SKU is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_product_doc() {}

/// Product details, including calculated current stock.
///
/// `stock` is `null` for services: not applicable, not zero.
#[utoipa::path(
    get,
    path = "/api/v1/products/{id}",
    tag = "catalog",
    params(("id" = Uuid, Path, description = "Product ID")),
    responses(
        (status = 200, description = "Product", body = ProductResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_product_doc() {}

/// Update only the fields sent.
#[utoipa::path(
    patch,
    path = "/api/v1/products/{id}",
    tag = "catalog",
    params(("id" = Uuid, Path, description = "Product ID")),
    request_body = UpdateProductRequest,
    responses(
        (status = 204, description = "Updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn update_product_doc() {}

/// Soft deletion. Idempotent. Never deletes: a product with movements retains
/// its history.
#[utoipa::path(
    post,
    path = "/api/v1/products/{id}/deactivate",
    tag = "catalog",
    params(("id" = Uuid, Path, description = "Product ID")),
    responses(
        (status = 204, description = "Deactivated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn deactivate_product_doc() {}

/// Movement history, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/products/{id}/movements",
    tag = "catalog",
    params(
        ("id" = Uuid, Path, description = "Product ID"),
        ListMovementsQuery,
    ),
    responses(
        (status = 200, description = "Movements", body = Vec<MovementResponse>),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_movements_doc() {}

/// Record a stock movement.
///
/// `quantity` is always positive; `reason` determines the sign. An outflow
/// that would leave stock negative is rejected with 409.
#[utoipa::path(
    post,
    path = "/api/v1/products/{id}/movements",
    tag = "catalog",
    params(("id" = Uuid, Path, description = "Product ID")),
    request_body = RecordMovementRequest,
    responses(
        (status = 201, description = "Movement recorded", body = IdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Product not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Insufficient stock", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input or service without stock", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn record_movement_doc() {}
