//! Customer account HTTP handlers.
//!
//! Authorization: the account routes require a session and nothing else, the
//! same rule the registry and the money ledger follow — collecting a debt and
//! reading statements or receivables are operational work a cashier or manager
//! does at the counter, not user administration.
//!
//! These routes live outside the customers module on purpose: each route is
//! served by whoever owns the rule it enforces, and the rules here belong to
//! the debt ledger rather than the customer registry.

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
use crate::customer_accounts::model::{
    AccountStatement, AccountStatementEntry, AdjustmentLogItem, AgingBuckets,
    CustomerAccountEntryKind, ReceiptLogItem, ReceivableCustomer, ReceivablesSummary,
    SaleOutstanding,
};
use crate::customer_accounts::service::{
    self, CollectedReceipt, ReceiptsError, RecordedAdjustment, RefundedCredit,
};
use crate::error::AppError;
use crate::extract::{AppJson, AppPath, AppQuery};

/// The published contract caps the idempotency key at this many characters.
/// utoipa attributes do not validate, and the ledger stores the key as text,
/// so the boundary is the only place the bound can be true.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/customers/{id}/account", get(get_account_statement))
        .route(
            "/customers/{id}/receipts",
            get(list_receipts).post(collect_receipt),
        )
        .route("/customers/{id}/credit-refunds", post(refund_credit))
        .route(
            "/customers/{id}/adjustments",
            get(list_adjustments).post(record_adjustment),
        )
        .route("/accounts/receivables", get(list_receivables))
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

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListAccountLogQuery {
    /// Maximum number of entries to return (default 50, clamped to 1..=200).
    pub limit: Option<i64>,
    /// Entries to skip from the newest (default 0).
    pub offset: Option<i64>,
}

/// Window over a portfolio report (`/accounts/receivables`).
///
/// Omitting `limit` returns the whole report, which is what the report screen
/// loads. The totals and aging describe the whole report either way; only the
/// `items` array is windowed.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListReportQuery {
    /// Maximum number of customers to return (default: all of them).
    pub limit: Option<i64>,
    /// Customers to skip from the first (default 0).
    pub offset: Option<i64>,
}

/// Window over the ledger entries of an account statement.
///
/// Omitting `limit` returns every entry, which is what the account panel needs
/// to pair each charge with its credit note. Passing it returns a bounded
/// window and `entries_total` still reports the whole ledger.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListStatementQuery {
    /// Maximum number of entries to return (default: all of them).
    pub limit: Option<i64>,
    /// Entries to skip from the oldest (default 0).
    pub offset: Option<i64>,
}

/// One ledger line in a customer's account statement, with the balance right
/// after it.
#[derive(Serialize, ToSchema)]
pub struct AccountStatementEntryResponse {
    pub id: Uuid,
    pub kind: CustomerAccountEntryKind,
    /// Signed effect on the balance: positive grows debt, negative shrinks it.
    #[schema(value_type = String, example = "150.00")]
    pub amount: String,
    /// Customer's running balance immediately after this entry.
    #[schema(value_type = String, example = "150.00")]
    pub running_balance: String,
    pub sale_id: Option<Uuid>,
    pub credit_note_id: Option<Uuid>,
    pub wallet_movement_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One sale that still owes money on the customer's account.
#[derive(Serialize, ToSchema)]
pub struct OutstandingSaleResponse {
    pub sale_id: Uuid,
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

/// Complete account statement for one customer: entries oldest first with a
/// running balance, current position, and every sale with debt.
#[derive(Serialize, ToSchema)]
pub struct CustomerAccountResponse {
    pub customer_id: Uuid,
    /// Current signed balance. Positive: the customer owes. Negative: credit.
    #[schema(value_type = String, example = "100.00")]
    pub balance: String,
    /// Collected money not yet allocated to a sale or refunded (`U`).
    #[schema(value_type = String, example = "0.00")]
    pub unapplied_credit: String,
    /// Sum of what every open sale still owes.
    #[schema(value_type = String, example = "100.00")]
    pub outstanding_total: String,
    /// Ledger entries oldest first, each carrying the running balance after it.
    /// Paginated only when `limit` is asked for; `entries_total` is the whole
    /// ledger either way.
    pub entries: Vec<AccountStatementEntryResponse>,
    /// Ledger entries in the whole statement, not just this page.
    pub entries_total: i64,
    /// Sales with `outstanding > 0`, oldest first.
    pub outstanding_sales: Vec<OutstandingSaleResponse>,
}

/// One allocation inside the receipt write log.
#[derive(Serialize, ToSchema)]
pub struct ReceiptLogAllocationResponse {
    pub sale_id: Uuid,
    #[schema(value_type = String, example = "50.00")]
    pub amount: String,
    pub created_at: DateTime<Utc>,
}

/// One collection entry returned by `GET /customers/{id}/receipts`.
#[derive(Serialize, ToSchema)]
pub struct ReceiptLogResponse {
    pub receipt_id: Uuid,
    pub wallet_movement_id: Uuid,
    pub wallet_id: Uuid,
    pub payment_method_id: Option<Uuid>,
    /// Positive amount collected.
    #[schema(value_type = String, example = "150.00")]
    pub amount: String,
    /// Sum of allocations pointing at this receipt.
    #[schema(value_type = String, example = "100.00")]
    pub allocated_total: String,
    pub allocations: Vec<ReceiptLogAllocationResponse>,
    pub note: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One manual adjustment returned by `GET /customers/{id}/adjustments`.
#[derive(Serialize, ToSchema)]
pub struct AdjustmentLogResponse {
    pub adjustment_id: Uuid,
    pub kind: CustomerAccountEntryKind,
    /// Signed effect on the balance: positive for `adjustment_in`, negative
    /// for `adjustment_out`.
    #[schema(value_type = String, example = "250.00")]
    pub amount: String,
    pub note: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Open sale debt split into aging buckets from each sale's `completed_at`
/// date (`0-30`, `31-60`, `61-90`, `90+`). No due date in this delivery.
#[derive(Serialize, ToSchema)]
pub struct AgingBucketsResponse {
    #[schema(value_type = String, example = "100.00")]
    pub days_0_30: String,
    #[schema(value_type = String, example = "50.00")]
    pub days_31_60: String,
    #[schema(value_type = String, example = "0.00")]
    pub days_61_90: String,
    #[schema(value_type = String, example = "25.00")]
    pub days_90_plus: String,
}

/// One customer with an open account position (`balance != 0` or
/// `unapplied_credit != 0`).
#[derive(Serialize, ToSchema)]
pub struct ReceivableCustomerResponse {
    pub customer_id: Uuid,
    pub customer_name: String,
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
    pub aging: AgingBucketsResponse,
}

/// Portfolio-wide receivables report across every customer with an open
/// account position.
#[derive(Serialize, ToSchema)]
pub struct ReceivablesResponse {
    #[schema(value_type = String, example = "175.00")]
    pub total_balance: String,
    #[schema(value_type = String, example = "0.00")]
    pub total_unapplied_credit: String,
    #[schema(value_type = String, example = "175.00")]
    pub total_outstanding: String,
    #[schema(value_type = String, example = "0.00")]
    pub total_adjustments: String,
    pub aging: AgingBucketsResponse,
    /// Customers on this page.
    pub items: Vec<ReceivableCustomerResponse>,
    /// Customers in the whole report, not just this page.
    pub items_total: i64,
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
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<CollectReceiptRequest>,
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
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<RefundCreditRequest>,
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
    AppPath(id): AppPath<Uuid>,
    AppJson(body): AppJson<RecordAdjustmentRequest>,
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

async fn get_account_statement(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListStatementQuery>,
) -> Result<Json<CustomerAccountResponse>, AppError> {
    let limit = query.limit.map(|limit| limit.clamp(1, 500));
    let offset = query.offset.unwrap_or(0).max(0);

    let statement = service::account_statement(&state.pool, id, limit, offset).await?;
    Ok(Json(to_account_response(statement)))
}

async fn list_receipts(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListAccountLogQuery>,
) -> Result<Json<Vec<ReceiptLogResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let receipts = service::list_receipts(&state.pool, id, limit, offset).await?;
    Ok(Json(receipts.into_iter().map(to_receipt_log).collect()))
}

async fn list_adjustments(
    State(state): State<AppState>,
    _user: AuthUser,
    AppPath(id): AppPath<Uuid>,
    AppQuery(query): AppQuery<ListAccountLogQuery>,
) -> Result<Json<Vec<AdjustmentLogResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let adjustments = service::list_adjustments(&state.pool, id, limit, offset).await?;
    Ok(Json(
        adjustments.into_iter().map(to_adjustment_log).collect(),
    ))
}

async fn list_receivables(
    State(state): State<AppState>,
    _user: AuthUser,
    AppQuery(query): AppQuery<ListReportQuery>,
) -> Result<Json<ReceivablesResponse>, AppError> {
    let limit = query.limit.map(|limit| limit.clamp(1, 200));
    let offset = query.offset.unwrap_or(0).max(0);

    let summary = service::receivables(&state.pool, limit, offset).await?;
    Ok(Json(to_receivables_response(summary)))
}

// --- Conversions --------------------------------------------------------

use crate::money::format_money;

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

fn to_statement_entry(item: AccountStatementEntry) -> AccountStatementEntryResponse {
    AccountStatementEntryResponse {
        id: item.entry.id,
        kind: item.entry.kind,
        amount: format_money(item.entry.amount),
        running_balance: format_money(item.running_balance),
        sale_id: item.entry.sale_id,
        credit_note_id: item.entry.credit_note_id,
        wallet_movement_id: item.entry.wallet_movement_id,
        note: item.entry.note,
        created_by: item.entry.created_by,
        created_at: item.entry.created_at,
    }
}

fn to_outstanding_sale(sale: SaleOutstanding) -> OutstandingSaleResponse {
    OutstandingSaleResponse {
        sale_id: sale.sale_id,
        completed_at: sale.completed_at,
        charged: format_money(sale.charged),
        cancelled: format_money(sale.cancelled),
        allocated: format_money(sale.allocated),
        outstanding: format_money(sale.outstanding),
    }
}

fn to_account_response(statement: AccountStatement) -> CustomerAccountResponse {
    CustomerAccountResponse {
        customer_id: statement.customer_id,
        balance: format_money(statement.position.balance),
        unapplied_credit: format_money(statement.position.unapplied_credit),
        outstanding_total: format_money(statement.position.outstanding_total),
        entries: statement
            .entries
            .into_iter()
            .map(to_statement_entry)
            .collect(),
        entries_total: statement.entries_total,
        outstanding_sales: statement
            .outstanding_sales
            .into_iter()
            .map(to_outstanding_sale)
            .collect(),
    }
}

fn to_receipt_log(item: ReceiptLogItem) -> ReceiptLogResponse {
    ReceiptLogResponse {
        receipt_id: item.entry_id,
        wallet_movement_id: item.wallet_movement_id,
        wallet_id: item.wallet_id,
        payment_method_id: item.payment_method_id,
        amount: format_money(item.amount),
        allocated_total: format_money(item.allocated_total),
        allocations: item
            .allocations
            .into_iter()
            .map(|alloc| ReceiptLogAllocationResponse {
                sale_id: alloc.sale_id,
                amount: format_money(alloc.amount),
                created_at: alloc.created_at,
            })
            .collect(),
        note: item.note,
        created_by: item.created_by,
        created_at: item.created_at,
    }
}

fn to_adjustment_log(item: AdjustmentLogItem) -> AdjustmentLogResponse {
    AdjustmentLogResponse {
        adjustment_id: item.entry_id,
        kind: item.kind,
        amount: format_money(item.amount),
        note: item.note,
        created_by: item.created_by,
        created_at: item.created_at,
    }
}

fn to_aging_response(aging: AgingBuckets) -> AgingBucketsResponse {
    AgingBucketsResponse {
        days_0_30: format_money(aging.days_0_30),
        days_31_60: format_money(aging.days_31_60),
        days_61_90: format_money(aging.days_61_90),
        days_90_plus: format_money(aging.days_90_plus),
    }
}

fn to_receivable_customer(item: ReceivableCustomer) -> ReceivableCustomerResponse {
    ReceivableCustomerResponse {
        customer_id: item.customer_id,
        customer_name: item.customer_name,
        tax_id: item.tax_id,
        is_active: item.is_active,
        balance: format_money(item.balance),
        unapplied_credit: format_money(item.unapplied_credit),
        outstanding_total: format_money(item.outstanding_total),
        adjustments: format_money(item.adjustments),
        aging: to_aging_response(item.aging),
    }
}

fn to_receivables_response(summary: ReceivablesSummary) -> ReceivablesResponse {
    ReceivablesResponse {
        total_balance: format_money(summary.total_balance),
        total_unapplied_credit: format_money(summary.total_unapplied_credit),
        total_outstanding: format_money(summary.total_outstanding),
        total_adjustments: format_money(summary.total_adjustments),
        aging: to_aging_response(summary.aging),
        items: summary
            .items
            .into_iter()
            .map(to_receivable_customer)
            .collect(),
        items_total: summary.items_total,
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

/// Customer account statement.
///
/// Returns ledger entries oldest first with their running balance, the
/// customer's current balance and unapplied credit (`U`), and every sale that
/// still owes money — all read from one consistent snapshot. Omitting `limit`
/// returns the whole ledger; `limit`/`offset` return a bounded window and
/// `entries_total` always reports the whole ledger.
#[utoipa::path(
    get,
    path = "/api/v1/customers/{id}/account",
    tag = "customer_accounts",
    params(
        ("id" = Uuid, Path, description = "Customer ID"),
        ListStatementQuery,
    ),
    responses(
        (status = 200, description = "Account statement", body = CustomerAccountResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn get_account_statement_doc() {}

/// Collection write log for one customer, newest first.
///
/// Accepts `limit` (default 50, clamped to 1..=200) and `offset` (default 0)
/// to page through the log.
#[utoipa::path(
    get,
    path = "/api/v1/customers/{id}/receipts",
    tag = "customer_accounts",
    params(
        ("id" = Uuid, Path, description = "Customer ID"),
        ListAccountLogQuery,
    ),
    responses(
        (status = 200, description = "Receipts recorded for this customer", body = [ReceiptLogResponse]),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_receipts_doc() {}

/// Manual debt adjustment write log for one customer, newest first.
///
/// Accepts `limit` (default 50, clamped to 1..=200) and `offset` (default 0)
/// to page through the log.
#[utoipa::path(
    get,
    path = "/api/v1/customers/{id}/adjustments",
    tag = "customer_accounts",
    params(
        ("id" = Uuid, Path, description = "Customer ID"),
        ListAccountLogQuery,
    ),
    responses(
        (status = 200, description = "Manual adjustments recorded for this customer", body = [AdjustmentLogResponse]),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
        (status = 404, description = "Unknown customer", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_adjustments_doc() {}

/// Accounts receivable report across every customer with an open position.
///
/// Lists every customer whose `balance != 0` or `unapplied_credit != 0`,
/// ordered by highest balance first, with open-sale aging buckets computed
/// from each sale's completion date (`0-30`, `31-60`, `61-90`, `90+`).
#[utoipa::path(
    get,
    path = "/api/v1/accounts/receivables",
    tag = "customer_accounts",
    params(ListReportQuery),
    responses(
        (status = 200, description = "Open customer positions and aging buckets", body = ReceivablesResponse),
        (status = 401, description = "No session", body = crate::openapi::Problem,
         content_type = "application/problem+json"),
    )
)]
#[allow(dead_code)]
fn list_receivables_doc() {}
