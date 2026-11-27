//! Supplier account HTTP handlers.
//!
//! Mirrors [`crate::customer_accounts::handlers`]: every route requires an
//! authenticated session (`AuthUser`), uses RFC 7807 `application/problem+json`
//! errors in English, and formats money fields as 2-decimal strings.

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
use crate::supplier_accounts::model::{
    AgingBuckets, PayableSupplier, PayablesSummary, PurchaseOutstanding, SupplierAccountEntryKind,
    SupplierAccountStatement, SupplierAccountStatementEntry, SupplierAdjustmentLogItem,
    SupplierPaymentLogItem,
};
use crate::supplier_accounts::service::{
    self, RecordedAdjustment, RecordedPayment, RefundedCredit, SupplierAccountError,
};

/// Maximum accepted length of an optional idempotency key in characters.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/suppliers/{id}/account", get(get_account_statement))
        .route(
            "/suppliers/{id}/payments",
            get(list_payments).post(record_payment),
        )
        .route("/suppliers/{id}/credit-refunds", post(refund_credit))
        .route(
            "/suppliers/{id}/adjustments",
            get(list_adjustments).post(record_adjustment),
        )
        .route("/accounts/payables", get(list_payables))
}

// --- DTOs ---------------------------------------------------------------

/// Which completed purchase one slice of a supplier payment applies to.
#[derive(Deserialize, ToSchema)]
pub struct SupplierPaymentAllocationRequest {
    pub purchase_id: Uuid,
    #[schema(value_type = String, example = "150.00")]
    pub amount: Decimal,
}

/// A payment to a supplier: money leaving a wallet, and the completed
/// purchases it pays.
///
/// Allocations are explicit and never invented by the server; any unallocated
/// remainder becomes unapplied advance credit with the supplier.
#[derive(Deserialize, ToSchema)]
pub struct RecordSupplierPaymentRequest {
    /// Optional retry key. A repeated payment with the same key returns the
    /// first payment with `200` instead of paying twice.
    #[schema(max_length = 100)]
    pub idempotency_key: Option<String>,
    pub payment_method_id: Uuid,
    #[schema(value_type = String, example = "150.00")]
    pub amount: Decimal,
    pub allocations: Vec<SupplierPaymentAllocationRequest>,
    pub note: Option<String>,
}

/// One allocated purchase with what it still owes after this payment.
#[derive(Serialize, ToSchema)]
pub struct SupplierPaymentAllocationResponse {
    pub purchase_id: Uuid,
    /// The purchase's outstanding payable after this payment's allocation.
    #[schema(value_type = String, example = "40.00")]
    pub outstanding: String,
}

#[derive(Serialize, ToSchema)]
pub struct SupplierPaymentResponse {
    /// The ledger entry that recorded the payment.
    pub payment_id: Uuid,
    /// The `supplier_account_payment` movement that carried the money out of
    /// the wallet.
    pub wallet_movement_id: Uuid,
    /// The supplier's balance after the payment. Positive: we still owe the
    /// supplier. Negative: we hold advance credit with them.
    #[schema(value_type = String, example = "40.00")]
    pub balance: String,
    /// Money paid to the supplier not yet allocated to a purchase or refunded.
    #[schema(value_type = String, example = "90.00")]
    pub unapplied_credit: String,
    pub allocations: Vec<SupplierPaymentAllocationResponse>,
}

/// A credit refund: unapplied advance credit returned by a supplier into a
/// wallet.
#[derive(Deserialize, ToSchema)]
pub struct RefundSupplierCreditRequest {
    pub payment_method_id: Uuid,
    #[schema(value_type = String, example = "50.00")]
    pub amount: Decimal,
    /// Why the supplier refunded the advance credit. Required and non-blank.
    pub note: String,
    /// Optional retry key. A repeated refund with the same key returns the
    /// first refund with `200` instead of booking the inflow twice.
    #[schema(max_length = 100)]
    pub idempotency_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SupplierCreditRefundResponse {
    /// The ledger entry that recorded the refund.
    pub refund_id: Uuid,
    /// The `supplier_account_refund` movement that carried the money into the
    /// wallet.
    pub wallet_movement_id: Uuid,
    /// The supplier's balance after the refund. Never positive after a refund.
    #[schema(value_type = String, example = "-20.00")]
    pub balance: String,
    /// Unapplied advance credit remaining after the refund.
    #[schema(value_type = String, example = "20.00")]
    pub unapplied_credit: String,
}

/// Direction of a manual payable adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupplierAdjustmentDirection {
    /// Increase what we owe the supplier (for example, an opening balance).
    In,
    /// Decrease what we owe the supplier without crossing below zero.
    Out,
}

impl SupplierAdjustmentDirection {
    fn into_kind(self) -> SupplierAccountEntryKind {
        match self {
            SupplierAdjustmentDirection::In => SupplierAccountEntryKind::AdjustmentIn,
            SupplierAdjustmentDirection::Out => SupplierAccountEntryKind::AdjustmentOut,
        }
    }
}

/// A manual adjustment against a supplier's payables ledger. Touches no wallet.
#[derive(Deserialize, ToSchema)]
pub struct RecordSupplierAdjustmentRequest {
    pub direction: SupplierAdjustmentDirection,
    #[schema(value_type = String, example = "250.00")]
    pub amount: Decimal,
    /// Why the balance was adjusted. Required and non-blank.
    pub note: String,
}

#[derive(Serialize, ToSchema)]
pub struct SupplierAdjustmentResponse {
    /// The ledger entry that recorded the adjustment.
    pub adjustment_id: Uuid,
    pub kind: SupplierAccountEntryKind,
    /// The supplier's balance after the adjustment. Always `>= 0.00`.
    #[schema(value_type = String, example = "250.00")]
    pub balance: String,
    #[schema(value_type = String, example = "0.00")]
    pub unapplied_credit: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSupplierAccountLogQuery {
    /// Maximum number of entries to return (default 50, clamped to 1..=200).
    pub limit: Option<i64>,
    /// Entries to skip from the newest (default 0).
    pub offset: Option<i64>,
}

/// Window over a portfolio report (`/accounts/payables`).
///
/// Omitting `limit` returns the whole report, which is what the report screen
/// loads. The totals and aging describe the whole report either way; only the
/// `items` array is windowed.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListReportQuery {
    /// Maximum number of suppliers to return (default: all of them).
    pub limit: Option<i64>,
    /// Suppliers to skip from the first (default 0).
    pub offset: Option<i64>,
}

/// Window over the ledger entries of a supplier account statement.
///
/// Omitting `limit` returns every entry, which is what the account panel needs
/// to pair each charge with its purchase return. Passing it returns a bounded
/// window and `entries_total` still reports the whole ledger.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListStatementQuery {
    /// Maximum number of entries to return (default: all of them).
    pub limit: Option<i64>,
    /// Entries to skip from the oldest (default 0).
    pub offset: Option<i64>,
}

/// One ledger line in a supplier's account statement, with the running balance
/// right after it.
#[derive(Serialize, ToSchema)]
pub struct SupplierAccountStatementEntryResponse {
    pub id: Uuid,
    pub kind: SupplierAccountEntryKind,
    /// Signed effect on the balance: positive grows what we owe, negative
    /// shrinks it.
    #[schema(value_type = String, example = "150.00")]
    pub amount: String,
    /// Supplier's running balance immediately after this entry.
    #[schema(value_type = String, example = "150.00")]
    pub running_balance: String,
    pub purchase_id: Option<Uuid>,
    pub purchase_return_id: Option<Uuid>,
    pub wallet_movement_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One completed purchase that still has an open payable on the supplier's
/// account.
#[derive(Serialize, ToSchema)]
pub struct OutstandingPurchaseResponse {
    pub purchase_id: Uuid,
    pub completed_at: DateTime<Utc>,
    #[schema(value_type = String, example = "200.00")]
    pub charged: String,
    #[schema(value_type = String, example = "20.00")]
    pub cancelled: String,
    #[schema(value_type = String, example = "80.00")]
    pub allocated: String,
    #[schema(value_type = String, example = "100.00")]
    pub outstanding: String,
}

/// Complete account statement for one supplier: entries oldest first with a
/// running balance, current position, and every purchase with open payable.
#[derive(Serialize, ToSchema)]
pub struct SupplierAccountResponse {
    pub supplier_id: Uuid,
    /// Current signed balance. Positive: we owe the supplier. Negative: we
    /// hold advance credit with them.
    #[schema(value_type = String, example = "100.00")]
    pub balance: String,
    /// Paid money not yet allocated to a purchase or refunded.
    #[schema(value_type = String, example = "0.00")]
    pub unapplied_credit: String,
    /// Sum of what every open purchase still owes.
    #[schema(value_type = String, example = "100.00")]
    pub outstanding_total: String,
    /// Ledger entries oldest first, each carrying the running balance after it.
    /// Paginated only when `limit` is asked for; `entries_total` is the whole
    /// ledger either way.
    pub entries: Vec<SupplierAccountStatementEntryResponse>,
    /// Ledger entries in the whole statement, not just this page.
    pub entries_total: i64,
    /// Purchases with `outstanding > 0`, oldest first.
    pub outstanding_purchases: Vec<OutstandingPurchaseResponse>,
}

/// One purchase allocation inside the supplier payment write log.
#[derive(Serialize, ToSchema)]
pub struct SupplierPaymentLogAllocationResponse {
    pub purchase_id: Uuid,
    #[schema(value_type = String, example = "50.00")]
    pub amount: String,
    pub created_at: DateTime<Utc>,
}

/// One supplier payment returned by `GET /suppliers/{id}/payments`.
#[derive(Serialize, ToSchema)]
pub struct SupplierPaymentLogResponse {
    pub payment_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub wallet_id: Uuid,
    pub payment_method_id: Option<Uuid>,
    /// Positive amount paid.
    #[schema(value_type = String, example = "150.00")]
    pub amount: String,
    /// Sum of allocations pointing at this payment.
    #[schema(value_type = String, example = "100.00")]
    pub allocated_total: String,
    pub allocations: Vec<SupplierPaymentLogAllocationResponse>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One manual adjustment returned by `GET /suppliers/{id}/adjustments`.
#[derive(Serialize, ToSchema)]
pub struct SupplierAdjustmentLogResponse {
    pub adjustment_id: Uuid,
    pub kind: SupplierAccountEntryKind,
    /// Signed effect on the balance: positive for `adjustment_in`, negative
    /// for `adjustment_out`.
    #[schema(value_type = String, example = "250.00")]
    pub amount: String,
    pub note: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Open purchase payables split into aging buckets from each purchase's
/// `completed_at` timestamp (`0-30`, `31-60`, `61-90`, `90+`).
#[derive(Serialize, ToSchema)]
pub struct PayablesAgingBucketsResponse {
    #[schema(value_type = String, example = "100.00")]
    pub days_0_30: String,
    #[schema(value_type = String, example = "50.00")]
    pub days_31_60: String,
    #[schema(value_type = String, example = "0.00")]
    pub days_61_90: String,
    #[schema(value_type = String, example = "25.00")]
    pub days_90_plus: String,
}

/// One supplier with an open account position (`balance != 0` or
/// `unapplied_credit != 0`).
#[derive(Serialize, ToSchema)]
pub struct PayableSupplierResponse {
    pub supplier_id: Uuid,
    pub supplier_name: String,
    pub tax_id: Option<String>,
    pub is_active: bool,
    #[schema(value_type = String, example = "175.00")]
    pub balance: String,
    #[schema(value_type = String, example = "0.00")]
    pub unapplied_credit: String,
    #[schema(value_type = String, example = "175.00")]
    pub outstanding_total: String,
    #[schema(value_type = String, example = "0.00")]
    pub adjustments: String,
    pub aging: PayablesAgingBucketsResponse,
}

/// Portfolio-wide payables report across every supplier with an open account
/// position.
#[derive(Serialize, ToSchema)]
pub struct PayablesResponse {
    #[schema(value_type = String, example = "175.00")]
    pub total_balance: String,
    #[schema(value_type = String, example = "0.00")]
    pub total_unapplied_credit: String,
    #[schema(value_type = String, example = "175.00")]
    pub total_outstanding: String,
    #[schema(value_type = String, example = "0.00")]
    pub total_adjustments: String,
    pub aging: PayablesAgingBucketsResponse,
    /// Suppliers on this page.
    pub items: Vec<PayableSupplierResponse>,
    /// Suppliers in the whole report, not just this page.
    pub items_total: i64,
}

// --- Handlers -----------------------------------------------------------

/// Normalize an optional idempotency key at the HTTP boundary: blank keys
/// become `None`; non-blank keys keep their exact characters and must be at
/// most `MAX_IDEMPOTENCY_KEY_LEN` characters.
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

async fn record_payment(
    State(state): State<AppState>,
    user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<RecordSupplierPaymentRequest>,
) -> Result<(StatusCode, Json<SupplierPaymentResponse>), AppError> {
    let allocations: Vec<(Uuid, Decimal)> = body
        .allocations
        .into_iter()
        .map(|allocation| (allocation.purchase_id, allocation.amount))
        .collect();

    let note = body
        .note
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());

    let idempotency_key = normalize_idempotency_key(body.idempotency_key)?;

    let recorded = service::record_payment(
        &state.pool,
        id,
        service::RecordPaymentInput {
            payment_method_id: body.payment_method_id,
            amount: body.amount,
            allocations: &allocations,
            idempotency_key: idempotency_key.as_deref(),
            note: note.as_deref(),
            actor: user.id(),
        },
    )
    .await?;

    let status = if recorded.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(to_payment_response(recorded))))
}

async fn refund_credit(
    State(state): State<AppState>,
    user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<RefundSupplierCreditRequest>,
) -> Result<(StatusCode, Json<SupplierCreditRefundResponse>), AppError> {
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
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<RecordSupplierAdjustmentRequest>,
) -> Result<(StatusCode, Json<SupplierAdjustmentResponse>), AppError> {
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

async fn get_account_statement(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListStatementQuery>,
) -> Result<Json<SupplierAccountResponse>, AppError> {
    let limit = query.limit.map(|limit| limit.clamp(1, 500));
    let offset = query.offset.unwrap_or(0).max(0);

    let statement = service::account_statement(&state.pool, id, limit, offset).await?;
    Ok(Json(to_account_response(statement)))
}

async fn list_payments(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListSupplierAccountLogQuery>,
) -> Result<Json<Vec<SupplierPaymentLogResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let payments = service::list_payments(&state.pool, id, limit, offset).await?;
    Ok(Json(payments.into_iter().map(to_payment_log).collect()))
}

async fn list_adjustments(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListSupplierAccountLogQuery>,
) -> Result<Json<Vec<SupplierAdjustmentLogResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let adjustments = service::list_adjustments(&state.pool, id, limit, offset).await?;
    Ok(Json(
        adjustments.into_iter().map(to_adjustment_log).collect(),
    ))
}

async fn list_payables(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListReportQuery>,
) -> Result<Json<PayablesResponse>, AppError> {
    let limit = query.limit.map(|limit| limit.clamp(1, 200));
    let offset = query.offset.unwrap_or(0).max(0);

    let summary = service::payables(&state.pool, limit, offset).await?;
    Ok(Json(to_payables_response(summary)))
}

// --- Conversions --------------------------------------------------------

use crate::money::format_money;

fn to_payment_response(recorded: RecordedPayment) -> SupplierPaymentResponse {
    SupplierPaymentResponse {
        payment_id: recorded.entry_id,
        wallet_movement_id: recorded.wallet_movement_id,
        balance: format_money(recorded.balance),
        unapplied_credit: format_money(recorded.unapplied_credit),
        allocations: recorded
            .allocations
            .into_iter()
            .map(
                |(purchase_id, outstanding)| SupplierPaymentAllocationResponse {
                    purchase_id,
                    outstanding: format_money(outstanding),
                },
            )
            .collect(),
    }
}

fn to_refund_response(refunded: RefundedCredit) -> SupplierCreditRefundResponse {
    SupplierCreditRefundResponse {
        refund_id: refunded.entry_id,
        wallet_movement_id: refunded.wallet_movement_id,
        balance: format_money(refunded.balance),
        unapplied_credit: format_money(refunded.unapplied_credit),
    }
}

fn to_adjustment_response(recorded: RecordedAdjustment) -> SupplierAdjustmentResponse {
    SupplierAdjustmentResponse {
        adjustment_id: recorded.entry_id,
        kind: recorded.kind,
        balance: format_money(recorded.balance),
        unapplied_credit: format_money(recorded.unapplied_credit),
    }
}

fn to_statement_entry(
    item: SupplierAccountStatementEntry,
) -> SupplierAccountStatementEntryResponse {
    SupplierAccountStatementEntryResponse {
        id: item.entry.id,
        kind: item.entry.kind,
        amount: format_money(item.entry.amount),
        running_balance: format_money(item.running_balance),
        purchase_id: item.entry.purchase_id,
        purchase_return_id: item.entry.purchase_return_id,
        wallet_movement_id: item.entry.wallet_movement_id,
        note: item.entry.note,
        created_by: item.entry.created_by,
        created_at: item.entry.created_at,
    }
}

fn to_outstanding_purchase(purchase: PurchaseOutstanding) -> OutstandingPurchaseResponse {
    OutstandingPurchaseResponse {
        purchase_id: purchase.purchase_id,
        completed_at: purchase.completed_at,
        charged: format_money(purchase.charged),
        cancelled: format_money(purchase.cancelled),
        allocated: format_money(purchase.allocated),
        outstanding: format_money(purchase.outstanding),
    }
}

fn to_account_response(statement: SupplierAccountStatement) -> SupplierAccountResponse {
    SupplierAccountResponse {
        supplier_id: statement.supplier_id,
        balance: format_money(statement.position.balance),
        unapplied_credit: format_money(statement.position.unapplied_credit),
        outstanding_total: format_money(statement.position.outstanding_total),
        entries: statement
            .entries
            .into_iter()
            .map(to_statement_entry)
            .collect(),
        entries_total: statement.entries_total,
        outstanding_purchases: statement
            .outstanding_purchases
            .into_iter()
            .map(to_outstanding_purchase)
            .collect(),
    }
}

fn to_payment_log(item: SupplierPaymentLogItem) -> SupplierPaymentLogResponse {
    SupplierPaymentLogResponse {
        payment_id: item.entry_id,
        wallet_movement_id: item.wallet_movement_id,
        wallet_id: item.wallet_id,
        payment_method_id: item.payment_method_id,
        amount: format_money(item.amount),
        allocated_total: format_money(item.allocated_total),
        allocations: item
            .allocations
            .into_iter()
            .map(|alloc| SupplierPaymentLogAllocationResponse {
                purchase_id: alloc.purchase_id,
                amount: format_money(alloc.amount),
                created_at: alloc.created_at,
            })
            .collect(),
        note: item.note,
        created_by: item.created_by,
        created_at: item.created_at,
    }
}

fn to_adjustment_log(item: SupplierAdjustmentLogItem) -> SupplierAdjustmentLogResponse {
    SupplierAdjustmentLogResponse {
        adjustment_id: item.entry_id,
        kind: item.kind,
        amount: format_money(item.amount),
        note: item.note,
        created_by: item.created_by,
        created_at: item.created_at,
    }
}

fn to_aging_response(aging: AgingBuckets) -> PayablesAgingBucketsResponse {
    PayablesAgingBucketsResponse {
        days_0_30: format_money(aging.days_0_30),
        days_31_60: format_money(aging.days_31_60),
        days_61_90: format_money(aging.days_61_90),
        days_90_plus: format_money(aging.days_90_plus),
    }
}

fn to_payable_supplier(item: PayableSupplier) -> PayableSupplierResponse {
    PayableSupplierResponse {
        supplier_id: item.supplier_id,
        supplier_name: item.supplier_name,
        tax_id: item.tax_id,
        is_active: item.is_active,
        balance: format_money(item.balance),
        unapplied_credit: format_money(item.unapplied_credit),
        outstanding_total: format_money(item.outstanding_total),
        adjustments: format_money(item.adjustments),
        aging: to_aging_response(item.aging),
    }
}

fn to_payables_response(summary: PayablesSummary) -> PayablesResponse {
    PayablesResponse {
        total_balance: format_money(summary.total_balance),
        total_unapplied_credit: format_money(summary.total_unapplied_credit),
        total_outstanding: format_money(summary.total_outstanding),
        total_adjustments: format_money(summary.total_adjustments),
        aging: to_aging_response(summary.aging),
        items: summary.items.into_iter().map(to_payable_supplier).collect(),
        items_total: summary.items_total,
    }
}

// --- Errors -------------------------------------------------------------

impl From<SupplierAccountError> for AppError {
    fn from(err: SupplierAccountError) -> Self {
        match err {
            SupplierAccountError::SupplierNotFound | SupplierAccountError::PurchaseNotFound => {
                AppError::NotFound
            }
            SupplierAccountError::SupplierBusy | SupplierAccountError::AlreadyRecorded => {
                AppError::Conflict(err.to_string())
            }
            SupplierAccountError::MethodUnavailable
            | SupplierAccountError::AmountTooLarge { .. }
            | SupplierAccountError::RefundNoteRequired
            | SupplierAccountError::Validation(_) => AppError::Validation(err.to_string()),
            SupplierAccountError::Internal(repo) => AppError::Internal(repo.to_string()),
        }
    }
}

// --- OpenAPI documentation ----------------------------------------------

/// Record a payment against a supplier's account.
///
/// One request, one transaction: money leaves the payment method's wallet as a
/// `supplier_account_payment` movement carrying the supplier and the method,
/// and the supplier's ledger records one `payment` entry linked to it.
/// Allocations are explicit — the server never invents them — and any
/// unallocated remainder becomes unapplied advance credit with the supplier.
/// An optional idempotency key makes a retry return the first payment with
/// `200` instead of paying twice.
#[utoipa::path(
    post,
    path = "/api/v1/suppliers/{id}/payments",
    tag = "supplier_accounts",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    request_body = RecordSupplierPaymentRequest,
    responses(
        (status = 201, description = "Payment recorded: wallet outflow and ledger entry written together",
         body = SupplierPaymentResponse),
        (status = 200, description = "Existing payment returned for a repeated idempotency key; nothing was written",
         body = SupplierPaymentResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier, or an allocation names an unknown purchase", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Busy supplier account, or idempotency key already used by another entry kind", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Unknown or inactive payment method, invalid amount, overlong idempotency key, or invalid allocations", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn record_payment_doc() {}

/// Record refunded advance credit returned by a supplier through a wallet
/// inflow.
///
/// One request, one transaction: a `supplier_account_refund` inflow enters the
/// payment method's wallet and a `credit_refund` entry moves the supplier's
/// balance toward zero. Refused unless `amount <= min(unapplied_credit, -balance)`.
/// An optional idempotency key returns the first refund with `200` instead of
/// booking the inflow twice.
#[utoipa::path(
    post,
    path = "/api/v1/suppliers/{id}/credit-refunds",
    tag = "supplier_accounts",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    request_body = RefundSupplierCreditRequest,
    responses(
        (status = 201, description = "Credit refund recorded: wallet inflow and ledger entry written together",
         body = SupplierCreditRefundResponse),
        (status = 200, description = "Existing credit refund returned for a repeated idempotency key; nothing was written",
         body = SupplierCreditRefundResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Busy supplier account, or idempotency key already used by another entry kind", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Unknown or inactive payment method, blank note, invalid amount, or amount exceeding refundable credit", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn refund_credit_doc() {}

/// Record a manual payable adjustment on a supplier's account.
///
/// Writes `adjustment_in` or `adjustment_out` without touching any wallet.
/// Both directions require the balance to stay `>= 0`: an adjustment may load
/// an opening payable or correct a balance, never create or consume advance
/// credit.
#[utoipa::path(
    post,
    path = "/api/v1/suppliers/{id}/adjustments",
    tag = "supplier_accounts",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    request_body = RecordSupplierAdjustmentRequest,
    responses(
        (status = 201, description = "Adjustment recorded", body = SupplierAdjustmentResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The supplier account is busy, retry", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Blank note, invalid amount, or adjustment crossing zero", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn record_adjustment_doc() {}

/// Supplier account statement.
///
/// Returns ledger entries oldest first with their running balance, the
/// supplier's current balance and unapplied credit, and every completed
/// purchase with an open payable — all read from one consistent snapshot.
/// Omitting `limit` returns the whole ledger; `limit`/`offset` return a bounded
/// window and `entries_total` always reports the whole ledger.
#[utoipa::path(
    get,
    path = "/api/v1/suppliers/{id}/account",
    tag = "supplier_accounts",
    params(
        ("id" = Uuid, Path, description = "Supplier ID"),
        ListStatementQuery,
    ),
    responses(
        (status = 200, description = "Supplier account statement", body = SupplierAccountResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_account_statement_doc() {}

/// Payment write log for one supplier, newest first.
///
/// Accepts `limit` (default 50, clamped to 1..=200) and `offset` (default 0)
/// to page through the log.
#[utoipa::path(
    get,
    path = "/api/v1/suppliers/{id}/payments",
    tag = "supplier_accounts",
    params(
        ("id" = Uuid, Path, description = "Supplier ID"),
        ListSupplierAccountLogQuery,
    ),
    responses(
        (status = 200, description = "Payments recorded for this supplier", body = [SupplierPaymentLogResponse]),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_payments_doc() {}

/// Manual payable adjustment write log for one supplier, newest first.
///
/// Accepts `limit` (default 50, clamped to 1..=200) and `offset` (default 0)
/// to page through the log.
#[utoipa::path(
    get,
    path = "/api/v1/suppliers/{id}/adjustments",
    tag = "supplier_accounts",
    params(
        ("id" = Uuid, Path, description = "Supplier ID"),
        ListSupplierAccountLogQuery,
    ),
    responses(
        (status = 200, description = "Manual adjustments recorded for this supplier", body = [SupplierAdjustmentLogResponse]),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown supplier", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_adjustments_doc() {}

/// Accounts payable report across every supplier with an open position.
///
/// Lists every supplier whose `balance != 0` or `unapplied_credit != 0`,
/// ordered by highest balance first, with open-purchase aging buckets computed
/// from each purchase's completion date (`0-30`, `31-60`, `61-90`, `90+`).
/// Omitting `limit` returns the whole report; `limit`/`offset` window `items`
/// while totals, aging, and `items_total` always describe the whole report.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/payables",
    tag = "supplier_accounts",
    params(ListReportQuery),
    responses(
        (status = 200, description = "Open supplier positions and aging buckets", body = PayablesResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_payables_doc() {}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn normalize_idempotency_key_discards_blank_and_enforces_limit() {
        assert_eq!(normalize_idempotency_key(None).unwrap(), None);
        assert_eq!(
            normalize_idempotency_key(Some("   ".to_string())).unwrap(),
            None
        );
        let padded = " key-1 ".to_string();
        assert_eq!(
            normalize_idempotency_key(Some(padded.clone())).unwrap(),
            Some(padded)
        );
        let max_key = "a".repeat(MAX_IDEMPOTENCY_KEY_LEN);
        assert_eq!(
            normalize_idempotency_key(Some(max_key.clone())).unwrap(),
            Some(max_key)
        );
        let too_long = "a".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1);
        assert!(matches!(
            normalize_idempotency_key(Some(too_long)),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn format_money_always_prints_two_decimals() {
        assert_eq!(format_money(dec!(0)), "0.00");
        assert_eq!(format_money(dec!(42.5)), "42.50");
        assert_eq!(format_money(dec!(-15.75)), "-15.75");
    }
}
