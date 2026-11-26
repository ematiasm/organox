//! Customers HTTP handlers.
//!
//! Authorization: the whole customers API requires a session. Registering and
//! editing customers is operational work (`manager`), not user administration, so
//! there is no additional role gate — the same rule the catalog follows.

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
use crate::customers::model::Customer;
use crate::customers::service::{self, CreateCustomerInput, CustomerError, UpdateCustomerInput};
use crate::error::AppError;
use crate::extract::{AppJson, AppPath, AppQuery};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/customers", get(list_customers).post(create_customer))
        .route("/customers/{id}", get(get_customer).patch(update_customer))
        .route("/customers/{id}/deactivate", post(deactivate_customer))
}

// --- DTOs ---------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct CustomerResponse {
    pub id: Uuid,
    pub name: String,
    /// `null` when the customer was registered without a document, which is a
    /// normal case: a walk-in buyer is still a customer.
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    /// `null` means no limit (the default), `"0.00"` means cash only. Money as
    /// a string with its cents, like every other amount in the tree.
    pub credit_limit: Option<String>,
    /// Current account balance derived from the customer's debt ledger.
    /// Positive means the customer owes money; negative means credit in their
    /// favour.
    #[schema(value_type = String, example = "120.00")]
    pub balance: String,
    /// Collected money not yet allocated to a sale or refunded (`U`).
    #[schema(value_type = String, example = "0.00")]
    pub available_credit: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct CustomerListResponse {
    pub items: Vec<CustomerResponse>,
    /// Page size actually applied (the request defaults to 50, is capped at 200).
    pub limit: i64,
    /// Offset of `items[0]` in the filtered set.
    pub offset: i64,
    /// How many customers the filters match in total, ignoring the page. A client
    /// needs this to tell a full page from the last one.
    pub total: i64,
}

#[derive(Serialize, ToSchema)]
pub struct CustomerIdResponse {
    pub id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateCustomerRequest {
    pub name: String,
    /// Optional and unique when present. An empty string means "no document".
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// Optional credit limit: absent or `null` means unlimited, `0` means
    /// cash only. Never negative, at most two decimals.
    #[schema(value_type = Option<String>, example = "100.00")]
    pub credit_limit: Option<Decimal>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateCustomerRequest {
    pub name: Option<String>,
    /// `null` means "leave the document as it is", never "clear it".
    pub tax_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub is_active: Option<bool>,
    /// The one clearable field: absent leaves the limit untouched, an explicit
    /// `null` returns the customer to unlimited, a value sets it. The double
    /// `Option` keeps the three cases apart the same way `PUT
    /// /sales/{id}/customer` keeps attach and clear apart.
    #[serde(default, deserialize_with = "present_decimal_option")]
    #[schema(value_type = Option<String>, example = "100.00")]
    pub credit_limit: Option<Option<Decimal>>,
}

/// Distinguishes an omitted field (`None`) from an explicit `null`
/// (`Some(None)`): serde collapses both to `None` for a plain `Option`, and
/// the limit needs the difference — omitted is "leave it as it is", `null` is
/// "back to unlimited".
fn present_decimal_option<'de, D>(deserializer: D) -> Result<Option<Option<Decimal>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Decimal>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListCustomersQuery {
    /// Include deactivated customers. Absent, the list shows only active ones.
    #[serde(default)]
    pub include_inactive: bool,
    /// Matches the name or the document, case-insensitively.
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// --- Handlers -----------------------------------------------------------

async fn list_customers(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListCustomersQuery>,
) -> Result<Json<CustomerListResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);

    let (items, total) = service::list_customers(
        &state.pool,
        query.include_inactive,
        query.search.as_deref(),
        limit,
        offset,
    )
    .await?;

    Ok(Json(CustomerListResponse {
        items: items.into_iter().map(to_response).collect(),
        limit,
        offset,
        total,
    }))
}

async fn create_customer(
    State(state): State<AppState>,
    _user: AuthUser,
    AppJson(body): AppJson<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<CustomerIdResponse>), AppError> {
    let id = service::create_customer(
        &state.pool,
        CreateCustomerInput {
            name: body.name,
            tax_id: body.tax_id,
            email: body.email,
            phone: body.phone,
            credit_limit: body.credit_limit,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(CustomerIdResponse { id })))
}

async fn get_customer(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<Json<CustomerResponse>, AppError> {
    let customer = service::get_customer(&state.pool, id).await?;
    Ok(Json(to_response(customer)))
}

async fn update_customer(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<UpdateCustomerRequest>,
) -> Result<StatusCode, AppError> {
    service::update_customer(
        &state.pool,
        id,
        UpdateCustomerInput {
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

async fn deactivate_customer(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
) -> Result<StatusCode, AppError> {
    service::deactivate_customer(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Conversions --------------------------------------------------------

use crate::money::format_money;

fn to_response(customer: Customer) -> CustomerResponse {
    CustomerResponse {
        id: customer.id,
        name: customer.name,
        tax_id: customer.tax_id,
        email: customer.email,
        phone: customer.phone,
        is_active: customer.is_active,
        credit_limit: customer.credit_limit.map(format_money),
        balance: format_money(customer.balance),
        available_credit: format_money(customer.available_credit),
        created_at: customer.created_at,
        updated_at: customer.updated_at,
    }
}

// --- Errors -------------------------------------------------------------

impl From<CustomerError> for AppError {
    fn from(err: CustomerError) -> Self {
        match err {
            CustomerError::Validation(validation) => AppError::Validation(validation.to_string()),
            CustomerError::TaxIdTaken => {
                AppError::Conflict("the document is already in use".to_string())
            }
            CustomerError::OpenAccountPosition { .. } | CustomerError::AccountBusy => {
                AppError::Conflict(err.to_string())
            }
            CustomerError::NotFound => AppError::NotFound,
            CustomerError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------
//
// Stubs exist only for `#[utoipa::path]` to generate the PathItem. The real
// routes are served by the handlers above.

/// List customers, newest first is not the order: alphabetical by name.
#[utoipa::path(
    get,
    path = "/api/v1/customers",
    tag = "customers",
    params(ListCustomersQuery),
    responses(
        (status = 200, description = "Customer page", body = CustomerListResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_customers_doc() {}

/// Register a customer.
///
/// `tax_id` is optional and unique when present; a duplicate returns 409.
/// `credit_limit` is optional too: absent or `null` means unlimited, `0`
/// means cash only, and a negative or three-decimal value is a 422.
#[utoipa::path(
    post,
    path = "/api/v1/customers",
    tag = "customers",
    request_body = CreateCustomerRequest,
    responses(
        (status = 201, description = "Customer created", body = CustomerIdResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Document is already in use", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_customer_doc() {}

/// Customer details.
#[utoipa::path(
    get,
    path = "/api/v1/customers/{id}",
    tag = "customers",
    params(("id" = Uuid, Path, description = "Customer ID")),
    responses(
        (status = 200, description = "Customer", body = CustomerResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_customer_doc() {}

/// Update only the fields sent.
///
/// A text field left out is not touched and cannot be cleared: there is no
/// body that removes a document, an email or a phone. `credit_limit` is the
/// one exception — an explicit `null` returns the customer to unlimited —
/// because it is the only field a real case has asked to clear.
#[utoipa::path(
    patch,
    path = "/api/v1/customers/{id}",
    tag = "customers",
    params(("id" = Uuid, Path, description = "Customer ID")),
    request_body = UpdateCustomerRequest,
    responses(
        (status = 204, description = "Updated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Document is already in use, account is busy, or deactivating a customer with an open balance or unapplied credit", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Invalid input", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn update_customer_doc() {}

/// Soft deletion. Idempotent. Never deletes: history points at the customer.
/// Refused with `409` while the customer carries a non-zero balance or
/// unapplied credit.
#[utoipa::path(
    post,
    path = "/api/v1/customers/{id}/deactivate",
    tag = "customers",
    params(("id" = Uuid, Path, description = "Customer ID")),
    responses(
        (status = 204, description = "Deactivated"),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Customer carries an open balance or unapplied credit, or the account is busy", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn deactivate_customer_doc() {}
