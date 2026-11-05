//! Handlers HTTP de ventas.
//!
//! El carrito vive en el servidor: cada acción devuelve la **venta completa**
//! recalculada. El cliente no suma ni valida nada — solo muestra. Es la
//! diferencia entre un total que se puede cobrar y uno que el navegador cree.

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
    /// Cantidad como string: `numeric(12,3)` no cabe en un float sin perder
    /// precisión en cantidades fraccionarias.
    pub quantity: String,
    pub unit_price: String,
    pub subtotal: String,
}

#[derive(Serialize, ToSchema)]
pub struct SaleResponse {
    pub id: Uuid,
    pub status: SaleStatus,
    pub items: Vec<SaleItemResponse>,
    /// Calculado en el servidor. El cliente lo muestra, no lo decide.
    pub total: String,
    pub item_count: usize,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct DraftResponse {
    pub id: Uuid,
    /// `true` si se reutilizó el borrador que ya estaba abierto.
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

// --- Conversiones -------------------------------------------------------

/// Dinero con 2 decimales exactos (`"1500.50"`), igual que en el catálogo.
fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

/// Cantidades sin ceros sobrantes (`"2"`, `"1.5"`).
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
                "stock insuficiente de {product_name}: disponible {}, pedido {}",
                format_quantity(available),
                format_quantity(requested)
            )),
            SalesError::NotEditable => AppError::Conflict(err.to_string()),
            SalesError::EmptySale => AppError::Validation(err.to_string()),
            SalesError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- Documentación OpenAPI ---------------------------------------------
//
// Stubs: existen solo para que `#[utoipa::path]` genere el PathItem.

/// Abre un borrador de venta, o devuelve el que ya estaba abierto.
///
/// Un usuario tiene **un** borrador a la vez: recargar la pantalla de venta no
/// crea carritos nuevos.
#[utoipa::path(
    post,
    path = "/api/v1/sales/draft",
    tag = "sales",
    responses(
        (status = 201, description = "Borrador nuevo", body = DraftResponse),
        (status = 200, description = "Se reutilizó el borrador abierto", body = DraftResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn open_draft_doc() {}

/// Venta con sus ítems y el total calculado por el servidor.
#[utoipa::path(
    get,
    path = "/api/v1/sales/{id}",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Id de la venta")),
    responses(
        (status = 200, description = "Venta", body = SaleResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "No existe", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_sale_doc() {}

/// Agrega un producto al borrador. Si ya estaba, suma la cantidad.
///
/// Devuelve la venta recalculada: el cliente no suma nada.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/items",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Id de la venta")),
    request_body = AddItemRequest,
    responses(
        (status = 200, description = "Venta actualizada", body = SaleResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Venta o producto inexistente", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "La venta ya está cerrada", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Cantidad inválida o el ítem es un servicio", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn add_item_doc() {}

/// Fija la cantidad de un ítem. Cantidad 0 lo quita.
#[utoipa::path(
    put,
    path = "/api/v1/sales/{id}/items/{product_id}",
    tag = "sales",
    params(
        ("id" = Uuid, Path, description = "Id de la venta"),
        ("product_id" = Uuid, Path, description = "Id del producto"),
    ),
    request_body = SetQuantityRequest,
    responses(
        (status = 200, description = "Venta actualizada", body = SaleResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Venta o producto inexistente", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "La venta ya está cerrada", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn set_quantity_doc() {}

/// Quita un ítem del borrador.
#[utoipa::path(
    delete,
    path = "/api/v1/sales/{id}/items/{product_id}",
    tag = "sales",
    params(
        ("id" = Uuid, Path, description = "Id de la venta"),
        ("product_id" = Uuid, Path, description = "Id del producto"),
    ),
    responses(
        (status = 200, description = "Venta actualizada", body = SaleResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Venta o producto inexistente", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn remove_item_doc() {}

/// Cobra la venta.
///
/// En una sola transacción: valida stock con la fila del producto bloqueada,
/// escribe un movimiento de salida por ítem y cierra la venta. Si falta stock,
/// rechaza con 409 y **no** deja movimientos ni cambia el estado.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/complete",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Id de la venta")),
    responses(
        (status = 200, description = "Venta cobrada", body = CompletedSaleResponse),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "No existe", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Stock insuficiente o venta ya cerrada", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "La venta está vacía", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn complete_sale_doc() {}

/// Cancela un borrador. No toca el libro de stock, porque nunca lo tocó.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/cancel",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Id de la venta")),
    responses(
        (status = 204, description = "Borrador cancelado"),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "No existe", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "La venta ya no es editable", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn cancel_sale_doc() {}

/// Últimas ventas cobradas.
#[utoipa::path(
    get,
    path = "/api/v1/sales",
    tag = "sales",
    responses(
        (status = 200, description = "Ventas completadas", body = Vec<SaleResponse>),
        (status = 401, description = "Sin sesión", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_sales_doc() {}
