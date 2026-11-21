//! Customer account HTTP handlers.
//!
//! Authorization: the account routes require a session and nothing else, the
//! same rule the registry and the money ledger follow — collecting a debt is
//! operational work a cashier or a manager does at the counter, not user
//! administration.
//!
//! One route so far: the collection. It is deliberately not part of the
//! customers module: the route is served by whoever owns the rule it
//! enforces, and the rule here is the account's, not the registry's.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::customer_accounts::service::{self, CollectedReceipt, ReceiptsError};
use crate::error::AppError;

/// The published contract caps the idempotency key at this many characters.
/// utoipa attributes do not validate, and the ledger stores the key as text,
/// so the boundary is the only place the bound can be true.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new().route("/customers/{id}/receipts", post(collect_receipt))
}

// --- DTOs ---------------------------------------------------------------

/// Which sale one slice of the collected money pays.
#[derive(Deserialize, ToSchema)]
pub struct ReceiptAllocationRequest {
    pub sale_id: Uuid,
    #[schema(value_type = String, example = "150.00")]
    pub amount: Decimal,
}

/// A collection: money that arrived, and the sales it pays.
///
/// The allocations are the operator's decision; the server never invents one
/// and never applies the remainder — that remainder becomes the customer's
/// usable credit exactly by carrying no row.
#[derive(Deserialize, ToSchema)]
pub struct CollectReceiptRequest {
    /// Optional retry key. A repeated collection with the same key returns
    /// the first receipt with `200` instead of collecting twice.
    #[schema(max_length = 100)]
    pub idempotency_key: Option<String>,
    pub payment_method_id: Uuid,
    #[schema(value_type = String, example = "150.00")]
    pub amount: Decimal,
    pub allocations: Vec<ReceiptAllocationRequest>,
    pub note: Option<String>,
}

/// One allocated sale with what it still owes after the receipt.
#[derive(Serialize, ToSchema)]
pub struct ReceiptAllocationResponse {
    pub sale_id: Uuid,
    /// The sale's outstanding amount after this receipt's allocation.
    #[schema(value_type = String, example = "40.00")]
    pub outstanding: String,
}

#[derive(Serialize, ToSchema)]
pub struct ReceiptResponse {
    /// The ledger entry that recorded the collection.
    pub receipt_id: Uuid,
    /// The `account_payment` movement that carried the money into the wallet.
    pub wallet_movement_id: Uuid,
    /// The customer's balance after the collection. Positive: they still
    /// owe. Negative: they hold money in their favour.
    #[schema(value_type = String, example = "40.00")]
    pub balance: String,
    /// Money collected that no allocation consumed yet, and no refund has
    /// returned: what the next sale on account may consume.
    #[schema(value_type = String, example = "90.00")]
    pub unapplied_credit: String,
    pub allocations: Vec<ReceiptAllocationResponse>,
}

// --- Handlers -----------------------------------------------------------

async fn collect_receipt(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CollectReceiptRequest>,
) -> Result<(StatusCode, Json<ReceiptResponse>), AppError> {
    let allocations: Vec<(Uuid, Decimal)> = body
        .allocations
        .into_iter()
        .map(|allocation| (allocation.sale_id, allocation.amount))
        .collect();

    // A blank note is "no note", never a note of spaces, the same
    // normalization the manual movements apply.
    let note = body
        .note
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());

    // A blank key is "no key", never a key of spaces: a key a client cannot
    // tell apart from another would make every later blank-key collection
    // answer with the first receipt — money collected and nothing booked.
    //
    // Only *blank* keys are discarded. A non-blank key keeps its exact bytes:
    // trimming it would rewrite the key namespace, so a receipt recorded before
    // this change under a padded key would stop matching its own retry (booking
    // a duplicate) and two padded variants would collapse onto one receipt. The
    // cap below measures what the client sent, never what we would have sent.
    let idempotency_key = body.idempotency_key.filter(|key| !key.trim().is_empty());
    if idempotency_key
        .as_ref()
        .is_some_and(|key| key.chars().count() > MAX_IDEMPOTENCY_KEY_LEN)
    {
        return Err(AppError::Validation(format!(
            "the idempotency key exceeds {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }

    let collected = service::collect(
        &state.pool,
        id,
        service::CollectInput {
            payment_method_id: body.payment_method_id,
            amount: body.amount,
            allocations: &allocations,
            idempotency_key: idempotency_key.as_deref(),
            note: note.as_deref(),
            actor: user.id(),
        },
    )
    .await?;

    // A replay did not collect anything, so answering 201 would make a client
    // count the same money twice; 200 says "this already happened".
    let status = if collected.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(to_response(collected))))
}

// --- Conversions --------------------------------------------------------

/// Money with exactly 2 decimals, negative included: `"-50.00"`.
fn format_money(value: Decimal) -> String {
    format!("{value:.2}")
}

fn to_response(collected: CollectedReceipt) -> ReceiptResponse {
    ReceiptResponse {
        receipt_id: collected.entry_id,
        wallet_movement_id: collected.wallet_movement_id,
        balance: format_money(collected.balance),
        unapplied_credit: format_money(collected.unapplied_credit),
        allocations: collected
            .allocations
            .into_iter()
            .map(|(sale_id, outstanding)| ReceiptAllocationResponse {
                sale_id,
                outstanding: format_money(outstanding),
            })
            .collect(),
    }
}

// --- Errors -------------------------------------------------------------

impl From<ReceiptsError> for AppError {
    fn from(err: ReceiptsError) -> Self {
        match err {
            ReceiptsError::CustomerNotFound | ReceiptsError::SaleNotFound => AppError::NotFound,
            // The busy answer mirrors sale completion's: the account is
            // serialized on purpose, and "retry" is the action.
            ReceiptsError::CustomerBusy => AppError::Conflict(err.to_string()),
            ReceiptsError::AlreadyRecorded => AppError::Conflict(err.to_string()),
            ReceiptsError::MethodUnavailable
            | ReceiptsError::AmountTooLarge { .. }
            | ReceiptsError::Validation(_) => AppError::Validation(err.to_string()),
            ReceiptsError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ----------------------------------------------

/// Collect money against a customer's account.
///
/// One request, one transaction: the money enters a wallet as a single
/// `account_payment` movement carrying the customer and the method, and the
/// customer's ledger records one receipt linked to it. The allocations are the
/// operator's — the server never invents or applies them — and the money no
/// allocation consumes becomes the customer's usable credit. An optional
/// idempotency key makes a retry return the first receipt with `200` instead
/// of collecting twice.
#[utoipa::path(
    post,
    path = "/api/v1/customers/{id}/receipts",
    tag = "customer_accounts",
    params(("id" = Uuid, Path, description = "Customer ID")),
    request_body = CollectReceiptRequest,
    responses(
        (status = 201, description = "Money collected: the wallet movement and the receipt are written together",
         body = ReceiptResponse),
        (status = 200, description = "Existing receipt returned for a repeated idempotency key; nothing was written",
         body = ReceiptResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer, or an allocation names an unknown sale", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The customer account is busy, retry", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Unknown or inactive payment method, a non-positive, too precise or too large amount, or allocations beyond what the sales owe", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn collect_receipt_doc() {}
