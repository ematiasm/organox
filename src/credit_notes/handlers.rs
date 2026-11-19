//! Credit-note HTTP handlers.
//!
//! Creating a note returns the full document: its lines, its total and the
//! refunds, so the client follows the money into its wallets without a second
//! request. Reading the same route reports what is left to return on the sale
//! and every note already issued against it.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::credit_notes::model::{CreditNoteDetail, ReturnLine};
use crate::credit_notes::service::{self, CreditNotesError};
use crate::error::AppError;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/sales/{id}/credit-notes",
        get(sale_credit_notes).post(create_credit_note),
    )
}

// --- DTOs ---------------------------------------------------------------

/// One line to return. The price is not sent: it is the one charged on the sale.
#[derive(Deserialize, ToSchema)]
pub struct CreditNoteLineRequest {
    pub product_id: Uuid,
    #[schema(value_type = String, example = "1")]
    pub quantity: Decimal,
}

/// A return. The total is not sent: the server calculates it.
#[derive(Deserialize, ToSchema)]
pub struct CreateCreditNoteRequest {
    /// Why the goods came back. Required, and kept on the document.
    pub reason: String,
    pub items: Vec<CreditNoteLineRequest>,
}

#[derive(Serialize, ToSchema)]
pub struct CreditNoteItemResponse {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: String,
    pub unit_price: String,
    pub subtotal: String,
}

/// Where the money went back.
#[derive(Serialize, ToSchema)]
pub struct CreditNoteRefundResponse {
    pub wallet_id: Uuid,
    pub wallet_name: String,
    pub payment_method_id: Uuid,
    pub payment_method_name: String,
    pub amount: String,
}

#[derive(Serialize, ToSchema)]
pub struct CreditNoteResponse {
    pub id: Uuid,
    /// The sale being corrected. The sale itself never changes.
    pub sale_id: Uuid,
    pub reason: String,
    /// Calculated by the server from the returned quantities.
    pub total: String,
    pub items: Vec<CreditNoteItemResponse>,
    pub refunds: Vec<CreditNoteRefundResponse>,
    pub created_at: DateTime<Utc>,
}

/// A charged line, with the quantity already credited and what can still come
/// back.
#[derive(Serialize, ToSchema)]
pub struct CreditableLineResponse {
    pub product_id: Uuid,
    pub product_name: String,
    pub quantity: String,
    pub unit_price: String,
    pub credited_quantity: String,
    pub returnable_quantity: String,
}

/// The sale's credits: its charged lines and the notes already issued.
#[derive(Serialize, ToSchema)]
pub struct SaleCreditNotesResponse {
    pub sale_id: Uuid,
    pub sale_total: String,
    pub refunded_total: String,
    pub items: Vec<CreditableLineResponse>,
    pub credit_notes: Vec<CreditNoteResponse>,
}

// --- Handlers -----------------------------------------------------------

async fn create_credit_note(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sale_id): Path<Uuid>,
    Json(body): Json<CreateCreditNoteRequest>,
) -> Result<(StatusCode, Json<CreditNoteResponse>), AppError> {
    let lines: Vec<ReturnLine> = body
        .items
        .into_iter()
        .map(|item| ReturnLine {
            product_id: item.product_id,
            quantity: item.quantity,
        })
        .collect();

    let note = service::create(&state.pool, sale_id, user.id(), &body.reason, &lines).await?;

    Ok((StatusCode::CREATED, Json(to_response(note))))
}

async fn sale_credit_notes(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(sale_id): Path<Uuid>,
) -> Result<Json<SaleCreditNotesResponse>, AppError> {
    let summary = service::for_sale(&state.pool, sale_id).await?;

    Ok(Json(SaleCreditNotesResponse {
        sale_id: summary.sale_id,
        sale_total: format_money(summary.sale_total),
        refunded_total: format_money(summary.refunded_total),
        items: summary
            .lines
            .into_iter()
            .map(|line| {
                let returnable = format_quantity(line.returnable_quantity());
                CreditableLineResponse {
                    product_id: line.product_id,
                    product_name: line.product_name,
                    quantity: format_quantity(line.quantity),
                    unit_price: format_money(line.unit_price),
                    credited_quantity: format_quantity(line.credited_quantity),
                    returnable_quantity: returnable,
                }
            })
            .collect(),
        credit_notes: summary.notes.into_iter().map(to_response).collect(),
    }))
}

// --- Mapping ------------------------------------------------------------

/// Money with exactly 2 decimals (`"1500.50"`), as everywhere else.
fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

/// Quantities without extra zeros (`"2"`, `"1.5"`).
fn format_quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

fn to_response(detail: CreditNoteDetail) -> CreditNoteResponse {
    CreditNoteResponse {
        id: detail.note.id,
        sale_id: detail.note.sale_id,
        reason: detail.note.reason,
        total: format_money(detail.note.total),
        items: detail
            .items
            .into_iter()
            .map(|item| {
                let subtotal = format_money(item.subtotal());
                CreditNoteItemResponse {
                    product_id: item.product_id,
                    product_name: item.product_name,
                    quantity: format_quantity(item.quantity),
                    unit_price: format_money(item.unit_price),
                    subtotal,
                }
            })
            .collect(),
        refunds: detail
            .refunds
            .into_iter()
            .map(|refund| CreditNoteRefundResponse {
                wallet_id: refund.wallet_id,
                wallet_name: refund.wallet_name,
                payment_method_id: refund.payment_method_id,
                payment_method_name: refund.payment_method_name,
                amount: format_money(refund.amount),
            })
            .collect(),
        created_at: detail.note.created_at,
    }
}

impl From<CreditNotesError> for AppError {
    fn from(err: CreditNotesError) -> Self {
        match err {
            CreditNotesError::SaleNotFound => AppError::NotFound,
            CreditNotesError::SaleNotCreditable => AppError::Conflict(err.to_string()),
            CreditNotesError::ExceedsReturnable {
                product_name,
                available,
                requested,
            } => AppError::Conflict(format!(
                "cannot return {} of {product_name}: only {} left to return",
                format_quantity(requested),
                format_quantity(available)
            )),
            CreditNotesError::Empty
            | CreditNotesError::QuantityNotPositive
            | CreditNotesError::QuantityPrecision
            | CreditNotesError::DuplicateProduct
            | CreditNotesError::ReasonMissing
            | CreditNotesError::ReasonTooLong
            | CreditNotesError::ProductNotInSale => AppError::Validation(err.to_string()),
            CreditNotesError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ---------------------------------------------

/// Return part or all of a charged sale.
///
/// Creates a credit note: the goods rejoin stock and the money leaves the
/// wallets that received the payment, in proportion to it. The sale is never
/// edited; a sale can be returned in several notes, and the total returned per
/// product can never exceed what was sold.
#[utoipa::path(
    post,
    path = "/api/v1/sales/{id}/credit-notes",
    tag = "credit_notes",
    params(("id" = Uuid, Path, description = "Sale ID")),
    request_body = CreateCreditNoteRequest,
    responses(
        (status = 201, description = "Credit note issued", body = CreditNoteResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Sale not completed, or more than what can still be returned",
         body = crate::openapi::Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid reason, line or quantity",
         body = crate::openapi::Problem, content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn create_credit_note_doc() {}

/// The credits of a sale: what was charged, what came back and where the money
/// of each note went. A draft has no stored total, so the charged lines decide
/// it; the refunded total is the sum of the notes already issued.
#[utoipa::path(
    get,
    path = "/api/v1/sales/{id}/credit-notes",
    tag = "credit_notes",
    params(("id" = Uuid, Path, description = "Sale ID")),
    responses(
        (status = 200, description = "Credits of the sale", body = SaleCreditNotesResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Sale not found", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_credit_notes_doc() {}
