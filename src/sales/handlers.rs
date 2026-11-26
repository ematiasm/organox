//! Sales HTTP handlers.
//!
//! The cart lives on the server: each action returns the recalculated **full
//! sale**. The client neither adds nor validates anything—it only displays.
//! This is the difference between a chargeable total and one the browser believes.

use axum::Json;
use axum::Router;
use axum::extract::State;
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
use crate::extract::{AppJson, AppPath};
use crate::sales::model::{Sale, SalePaymentInput, SaleStatus};
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
        .route("/sales/{id}/customer", put(set_sale_customer))
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
    /// The customer the sale is charged to, `null` for an anonymous sale.
    pub customer_id: Option<Uuid>,
    /// The live registry name while the sale is a draft; the snapshot frozen
    /// at completion once closed, so a later rename changes nothing.
    pub customer_name: Option<String>,
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
    /// How much of the on-account part the customer's available credit paid
    /// automatically: `on_account - credit_applied` is the debt the sale
    /// actually added. Money with two decimals, like `total`; `"0.00"` for
    /// a sale that never used the account.
    pub credit_applied: String,
    pub movement_ids: Vec<Uuid>,
    /// One movement per payment, so the client can follow the money into its
    /// wallets without a second request.
    pub payment_movement_ids: Vec<Uuid>,
    pub completed_at: DateTime<Utc>,
}

/// One payment of the charge. A sale can be paid with several.
#[derive(Deserialize, ToSchema)]
pub struct SalePaymentRequest {
    pub payment_method_id: Uuid,
    /// Positive. The sum of every payment must equal the total the server
    /// calculated, or the charge is rejected with 422.
    #[schema(value_type = String, example = "1500.00")]
    pub amount: Decimal,
}

/// How the sale is paid. Required: a sale never closes without saying where the
/// money went.
#[derive(Deserialize, ToSchema)]
pub struct CompleteSaleRequest {
    pub payments: Vec<SalePaymentRequest>,
    /// Part of the total charged to the customer's account instead of
    /// collected at the till. Optional and defaults to 0; never negative and
    /// at most two decimals. The payments plus this amount must equal the
    /// total the server calculated, and a positive amount needs an active
    /// customer on the draft whose credit limit still fits it.
    #[serde(default)]
    #[schema(value_type = String, example = "60.00")]
    pub on_account: Decimal,
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

/// Attaches a customer to the draft, or clears it.
///
/// `customer_id` must be present: clearing is an explicit `null`, and an omitted
/// field is rejected instead of being read as "clear". A truncated payload
/// would otherwise detach a customer silently, and the cashier would see it only
/// on the printed ticket.
#[derive(Deserialize, ToSchema)]
pub struct SetSaleCustomerRequest {
    /// The customer to attach, or `null` to leave the sale anonymous.
    #[serde(default, deserialize_with = "present_option")]
    #[schema(value_type = Option<Uuid>)]
    pub customer_id: Option<Option<Uuid>>,
}

/// Distinguishes an omitted field (`None`) from an explicit `null`
/// (`Some(None)`): serde collapses both to `None` for a plain `Option`.
fn present_option<'de, D>(deserializer: D) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}

// --- Handlers -----------------------------------------------------------

async fn open_draft(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<(StatusCode, Json<DraftResponse>), AppError> {
    let (id, reused) = service::open_draft(&state.pool, user.id()).await?;

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
    AppPath(id): AppPath<Uuid>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::get(&state.pool, id).await?;
    Ok(Json(to_response(sale)))
}

async fn add_item(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<AddItemRequest>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::add_item(&state.pool, id, body.product_id, body.quantity).await?;
    Ok(Json(to_response(sale)))
}

async fn set_quantity(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath((id, product_id)): AppPath<(Uuid, Uuid)>,
    AppJson(body): AppJson<SetQuantityRequest>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::set_item_quantity(&state.pool, id, product_id, body.quantity).await?;
    Ok(Json(to_response(sale)))
}

async fn remove_item(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath((id, product_id)): AppPath<(Uuid, Uuid)>,
) -> Result<Json<SaleResponse>, AppError> {
    let sale = service::remove_item(&state.pool, id, product_id).await?;
    Ok(Json(to_response(sale)))
}

async fn complete_sale(
    State(state): State<AppState>,
    user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<CompleteSaleRequest>,
) -> Result<Json<CompletedSaleResponse>, AppError> {
    let payments: Vec<SalePaymentInput> = body
        .payments
        .into_iter()
        .map(|payment| SalePaymentInput {
            payment_method_id: payment.payment_method_id,
            amount: payment.amount,
        })
        .collect();

    let done =
        service::complete_with_on_account(&state.pool, id, user.id(), &payments, body.on_account)
            .await?;
    Ok(Json(CompletedSaleResponse {
        sale_id: done.sale_id,
        total: format_money(done.total),
        credit_applied: format_money(done.credit_applied),
        movement_ids: done.movement_ids,
        payment_movement_ids: done.payment_movement_ids,
        completed_at: done.completed_at,
    }))
}

async fn cancel_sale(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<StatusCode, AppError> {
    service::cancel(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_sale_customer(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<SetSaleCustomerRequest>,
) -> Result<StatusCode, AppError> {
    let Some(customer_id) = body.customer_id else {
        return Err(AppError::Validation(
            "customer_id is required: send null to clear the sale's customer".to_string(),
        ));
    };

    service::set_customer(&state.pool, id, customer_id).await?;
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

use crate::money::format_money;

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
        customer_id: sale.customer_id,
        customer_name: sale.customer_name,
        created_at: sale.created_at,
        completed_at: sale.completed_at,
    }
}

impl From<SalesError> for AppError {
    fn from(err: SalesError) -> Self {
        match err {
            SalesError::NotFound => AppError::NotFound,
            SalesError::ProductNotFound => AppError::NotFound,
            SalesError::ServiceNotSellable
            | SalesError::QuantityNotPositive
            | SalesError::QuantityPrecision
            | SalesError::QuantityTooLarge
            | SalesError::TotalTooLarge => AppError::Validation(err.to_string()),
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
            SalesError::PaymentAmountNotPositive
            | SalesError::PaymentAmountPrecision
            | SalesError::PaymentAmountTooLarge
            | SalesError::PaymentMethodUnavailable
            | SalesError::OnAccountNegative
            | SalesError::OnAccountPrecision
            | SalesError::OnAccountTooLarge
            | SalesError::OnAccountNeedsCustomer
            | SalesError::OnAccountNeedsActiveCustomer => AppError::Validation(err.to_string()),
            SalesError::OnAccountExceedsCreditLimit { available } => {
                // The same family as the stock conflict: the request can be
                // retried with a smaller on-account part, and the message
                // names by how much. Money always carries its cents, even
                // when what remains is zero.
                AppError::Conflict(format!(
                    "the on-account amount exceeds the {} the customer still has available",
                    format_money(available)
                ))
            }
            SalesError::PaymentMismatch {
                total,
                paid,
                on_account,
            } => {
                // The wording is part of the contract. A completion that never
                // mentions the account keeps the sentence it has always had;
                // only a request that actually used it may say so.
                let detail = if on_account > Decimal::ZERO {
                    format!(
                        "the payments and the on-account amount add up to {}, but the sale total is {}",
                        format_money(paid),
                        format_money(total)
                    )
                } else {
                    "the payments do not add up to the sale total".to_string()
                };
                AppError::Validation(detail)
            }
            SalesError::CustomerInactive => AppError::Conflict(err.to_string()),
            SalesError::CustomerAccountBusy => AppError::Conflict(err.to_string()),
            SalesError::CustomerLookup(err) => AppError::Internal(err.to_string()),
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
/// In one transaction: validate stock with the product row locked, check that
/// the payments plus the optional `on_account` amount add up to the total,
/// authorize the on-account part against the customer's activity and credit
/// limit, write an outflow movement per item, a money movement per payment
/// and, when part of the total is on account, the charge in the customer's
/// ledger — which their available credit then pays, oldest receipt first, up
/// to the charged part — then close the sale. Any rejection leaves **no**
/// movements, no ledger entry and no status change.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/complete",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    request_body = CompleteSaleRequest,
    responses(
        (status = 200, description = "Sale completed", body = CompletedSaleResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Insufficient stock, sale already closed, busy customer account, or an on-account amount beyond the customer's credit limit (the remaining credit is named in the message)", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Sale empty, wrong sum, unusable method, or an on-account amount without an active customer", body = crate::openapi::Problem,
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

/// Attach or clear the sale's customer. Draft-only.
///
/// `customer_id` is required in the body; `null` clears the customer. A closed
/// sale is a fact and takes no customer (409). The customer must exist (404) and
/// be active (409): those are the customers module's rules, not a copy in sales.
#[utoipa::path(
    put,
    path = "/api/v1/sales/{id}/customer",
    tag = "sales",
    params(("id" = Uuid, Path, description = "Sale ID")),
    request_body = SetSaleCustomerRequest,
    responses(
        (status = 204, description = "Customer attached or cleared"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale or customer not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Sale is closed, or the customer is deactivated", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "customer_id is missing", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn set_sale_customer_doc() {}

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
