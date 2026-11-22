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
use crate::customer_accounts::model::CustomerAccountEntryKind;
use crate::customer_accounts::service::{
    self, CollectedReceipt, ReceiptsError, RecordedAdjustment, RefundedCredit,
};
use crate::error::AppError;

/// The published contract caps the idempotency key at this many characters.
/// utoipa attributes do not validate, and the ledger stores the key as text,
/// so the boundary is the only place the bound can be true.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/customers/{id}/receipts", post(collect_receipt))
        .route("/customers/{id}/credit-refunds", post(refund_credit))
        .route("/customers/{id}/adjustments", post(record_adjustment))
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

/// A credit refund: money handed back to a customer who holds usable credit.
#[derive(Deserialize, ToSchema)]
pub struct RefundCreditRequest {
    pub payment_method_id: Uuid,
    #[schema(value_type = String, example = "50.00")]
    pub amount: Decimal,
    /// Why the money is being handed back. Required and non-blank.
    pub note: String,
    /// Optional retry key. A repeated refund with the same key returns the
    /// first refund with `200` instead of handing the money back twice.
    #[schema(max_length = 100)]
    pub idempotency_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CreditRefundResponse {
    /// The ledger entry that recorded the refund.
    pub refund_id: Uuid,
    /// The `account_payment_refund` movement that carried the money out of the
    /// wallet.
    pub wallet_movement_id: Uuid,
    /// The customer's balance after the refund. Never positive after a refund.
    #[schema(value_type = String, example = "-20.00")]
    pub balance: String,
    /// Usable credit remaining after the refund.
    #[schema(value_type = String, example = "20.00")]
    pub unapplied_credit: String,
}

/// Direction of a manual debt adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentDirection {
    /// Increase the customer's debt (for example, loading an opening balance).
    In,
    /// Decrease the customer's debt without crossing below zero.
    Out,
}

impl AdjustmentDirection {
    fn into_kind(self) -> CustomerAccountEntryKind {
        match self {
            AdjustmentDirection::In => CustomerAccountEntryKind::AdjustmentIn,
            AdjustmentDirection::Out => CustomerAccountEntryKind::AdjustmentOut,
        }
    }
}

/// A manual adjustment against a customer's debt ledger. Touches no wallet.
#[derive(Deserialize, ToSchema)]
pub struct RecordAdjustmentRequest {
    pub direction: AdjustmentDirection,
    #[schema(value_type = String, example = "250.00")]
    pub amount: Decimal,
    /// Why the balance was adjusted. Required and non-blank.
    pub note: String,
}

#[derive(Serialize, ToSchema)]
pub struct AdjustmentResponse {
    /// The ledger entry that recorded the adjustment.
    pub adjustment_id: Uuid,
    pub kind: CustomerAccountEntryKind,
    /// The customer's balance after the adjustment. Always `>= 0.00`.
    #[schema(value_type = String, example = "250.00")]
    pub balance: String,
    #[schema(value_type = String, example = "0.00")]
    pub unapplied_credit: String,
}

// --- Handlers -----------------------------------------------------------

/// Normalize an optional idempotency key at the HTTP boundary.
///
/// Only *blank* keys are discarded. A non-blank key keeps exactly what the
/// client sent: trimming it would rewrite the key namespace, so a record
/// saved under a padded key would stop matching its own retry. The cap counts
/// characters, matching the contract's wording.
fn normalize_idempotency_key(raw: Option<String>) -> Result<Option<String>, AppError> {
    let key = raw.filter(|key| !key.trim().is_empty());
    if key
        .as_ref()
        .is_some_and(|key| key.chars().count() > MAX_IDEMPOTENCY_KEY_LEN)
    {
        return Err(AppError::Validation(format!(
            "the idempotency key exceeds {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }
    Ok(key)
}

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

    let idempotency_key = normalize_idempotency_key(body.idempotency_key)?;

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

async fn refund_credit(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RefundCreditRequest>,
) -> Result<(StatusCode, Json<CreditRefundResponse>), AppError> {
    let idempotency_key = normalize_idempotency_key(body.idempotency_key)?;

    let refunded = service::refund_credit(
        &state.pool,
        id,
        service::RefundCreditInput {
            payment_method_id: body.payment_method_id,
            amount: body.amount,
            note: &body.note,
            idempotency_key: idempotency_key.as_deref(),
            actor: user.id(),
        },
    )
    .await?;

    let status = if refunded.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(to_refund_response(refunded))))
}

async fn record_adjustment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RecordAdjustmentRequest>,
) -> Result<(StatusCode, Json<AdjustmentResponse>), AppError> {
    let recorded = service::adjust(
        &state.pool,
        id,
        service::AdjustInput {
            kind: body.direction.into_kind(),
            amount: body.amount,
            note: &body.note,
            actor: user.id(),
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(to_adjustment_response(recorded))))
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

fn to_refund_response(refunded: RefundedCredit) -> CreditRefundResponse {
    CreditRefundResponse {
        refund_id: refunded.entry_id,
        wallet_movement_id: refunded.wallet_movement_id,
        balance: format_money(refunded.balance),
        unapplied_credit: format_money(refunded.unapplied_credit),
    }
}

fn to_adjustment_response(recorded: RecordedAdjustment) -> AdjustmentResponse {
    AdjustmentResponse {
        adjustment_id: recorded.entry_id,
        kind: recorded.kind,
        balance: format_money(recorded.balance),
        unapplied_credit: format_money(recorded.unapplied_credit),
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
            | ReceiptsError::RefundNoteRequired
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

/// Hand collected credit back to a customer through a wallet movement.
///
/// One request, one transaction: an `account_payment_refund` outflow leaves
/// the method's wallet and a `credit_refund` entry moves the customer's
/// balance toward zero. Refused unless `amount <= unapplied_credit` and
/// `balance + amount <= 0`. An optional idempotency key returns the first
/// refund with `200` instead of handing money back twice.
#[utoipa::path(
    post,
    path = "/api/v1/customers/{id}/credit-refunds",
    tag = "customer_accounts",
    params(("id" = Uuid, Path, description = "Customer ID")),
    request_body = RefundCreditRequest,
    responses(
        (status = 201, description = "Credit refunded: the wallet movement and ledger entry are written together",
         body = CreditRefundResponse),
        (status = 200, description = "Existing credit refund returned for a repeated idempotency key; nothing was written",
         body = CreditRefundResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Busy customer account, or idempotency key already used by another entry kind", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Unknown or inactive payment method, blank note, invalid amount, or amount exceeding available credit", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn refund_credit_doc() {}

/// Record a manual debt adjustment on a customer's account.
///
/// Writes `adjustment_in` or `adjustment_out` without touching any wallet.
/// Both directions require the balance to stay `>= 0`: an adjustment may
/// load an opening balance or correct a debt, never create or consume
/// available credit.
#[utoipa::path(
    post,
    path = "/api/v1/customers/{id}/adjustments",
    tag = "customer_accounts",
    params(("id" = Uuid, Path, description = "Customer ID")),
    request_body = RecordAdjustmentRequest,
    responses(
        (status = 201, description = "Adjustment recorded", body = AdjustmentResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The customer account is busy, retry", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Blank note, invalid amount, or adjustment crossing zero", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn record_adjustment_doc() {}
