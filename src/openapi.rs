//! OpenAPI 3.1 specification generated from code.
//!
//! # Why it exists
//!
//! The frontend consumes TypeScript types **generated** from this spec, not
//! handwritten. If the backend changes a field, `schema.d.ts` changes and
//! `svelte-check` fails. Without this, the contract lives in the head of the
//! person who wrote the handler.
//!
//! # The rule the spec must respect
//!
//! Decimals are **strings** in both directions:
//!
//! - **Output**: `price`, `stock`, `min_stock`, `quantity`, `unit_cost` are
//!   `String` in DTOs. An `f64` in JSON loses monetary precision.
//! - **Input**: the server *also* accepts numbers (`rust_decimal` deserializes
//!   them), but the spec documents **only strings** because that is the
//!   contract the frontend must use. Accepting numbers is parsing tolerance,
//!   not part of the contract.
//!   See `tests/catalog.rs::decimal_inputs_accept_both_string_and_number`.
//!
//! # Errors
//!
//! All error responses use `application/problem+json` (RFC 7807) with the
//! [`Problem`] schema. `429` adds the `Retry-After` header.

use utoipa::OpenApi;

/// RFC 7807 error body. Same shape for every status code.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub struct Problem {
    /// URI identifying the problem type. Currently always `about:blank`.
    #[schema(example = "about:blank")]
    pub r#type: String,
    /// Human-readable, stable summary of the problem.
    #[schema(example = "Unauthorized")]
    pub title: String,
    /// HTTP status, repeated in the body for clients that do not read headers.
    #[schema(example = 401)]
    pub status: u16,
    /// Actionable explanation for the user.
    #[schema(example = "Invalid credentials or expired session.")]
    pub detail: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "organox",
        version = env!("CARGO_PKG_VERSION"),
        description = "Business administration for a small or medium enterprise (stock and supplies). \
                       Single-tenant.\n\n\
                       **Authentication**: except `POST /api/v1/auth/login`, every route \
                       requires the `session` cookie (HttpOnly, SameSite=Strict). \
                       Without it, routes return `401`.\n\n\
                       **Errors**: all error responses use \
                       `application/problem+json` (RFC 7807). A `429` also includes \
                       the `Retry-After` header in seconds.\n\n\
                       **Decimals**: always transmitted as **strings** (`\"1500.50\"`, \
                       `\"12.5\"`), never numbers, to avoid losing precision."
    ),
    paths(
        crate::auth::handlers::login,
        crate::auth::handlers::logout,
        crate::auth::handlers::me,
        crate::auth::handlers::list_users,
        crate::auth::handlers::create_user,
        crate::auth::handlers::change_password,
        crate::auth::handlers::reset_password,
        crate::catalog::handlers::list_products,
        crate::catalog::handlers::create_product,
        crate::catalog::handlers::get_product,
        crate::catalog::handlers::update_product,
        crate::catalog::handlers::deactivate_product,
        crate::catalog::handlers::list_movements,
        crate::catalog::handlers::record_movement,
        crate::customers::handlers::list_customers,
        crate::customers::handlers::create_customer,
        crate::customers::handlers::get_customer,
        crate::customers::handlers::update_customer,
        crate::customers::handlers::deactivate_customer,
        crate::customer_accounts::handlers::get_account_statement,
        crate::customer_accounts::handlers::list_receipts,
        crate::customer_accounts::handlers::collect_receipt,
        crate::customer_accounts::handlers::refund_credit,
        crate::customer_accounts::handlers::list_adjustments,
        crate::customer_accounts::handlers::record_adjustment,
        crate::customer_accounts::handlers::list_receivables,
        crate::sales::handlers::open_draft,
        crate::sales::handlers::get_sale,
        crate::sales::handlers::add_item,
        crate::sales::handlers::set_quantity,
        crate::sales::handlers::remove_item,
        crate::sales::handlers::complete_sale,
        crate::sales::handlers::cancel_sale,
        crate::sales::handlers::set_sale_customer,
        crate::sales::handlers::list_sales,
        crate::credit_notes::handlers::create_credit_note_doc,
        crate::credit_notes::handlers::get_credit_notes_doc,
        crate::payments::handlers::list_wallets_doc,
        crate::payments::handlers::create_wallet_doc,
        crate::payments::handlers::get_wallet_doc,
        crate::payments::handlers::update_wallet_doc,
        crate::payments::handlers::list_wallet_movements_doc,
        crate::payments::handlers::record_manual_movement_doc,
        crate::payments::handlers::list_payment_methods_doc,
        crate::payments::handlers::create_payment_method_doc,
        crate::payments::handlers::update_payment_method_doc,
        crate::suppliers::handlers::list_suppliers_doc,
        crate::suppliers::handlers::create_supplier_doc,
        crate::suppliers::handlers::get_supplier_doc,
        crate::suppliers::handlers::update_supplier_doc,
        crate::suppliers::handlers::deactivate_supplier_doc,
        crate::supplier_accounts::handlers::get_account_statement_doc,
        crate::supplier_accounts::handlers::list_payments_doc,
        crate::supplier_accounts::handlers::record_payment_doc,
        crate::supplier_accounts::handlers::refund_credit_doc,
        crate::supplier_accounts::handlers::list_adjustments_doc,
        crate::supplier_accounts::handlers::record_adjustment_doc,
        crate::supplier_accounts::handlers::list_payables_doc,
        crate::purchases::handlers::open_draft_doc,
        crate::purchases::handlers::list_purchases_doc,
        crate::purchases::handlers::get_purchase_doc,
        crate::purchases::handlers::set_purchase_supplier_doc,
        crate::purchases::handlers::add_item_doc,
        crate::purchases::handlers::set_item_doc,
        crate::purchases::handlers::remove_item_doc,
        crate::purchases::handlers::complete_purchase_doc,
        crate::purchases::handlers::cancel_purchase_doc,
        crate::purchase_returns::handlers::create_purchase_return_doc,
        crate::purchase_returns::handlers::get_purchase_returns_doc,
    ),
    components(schemas(
        Problem,
        crate::auth::handlers::LoginRequest,
        crate::auth::handlers::UserResponse,
        crate::auth::handlers::LoginResponse,
        crate::auth::handlers::CreateUserRequest,
        crate::auth::handlers::CreateUserResponse,
        crate::auth::handlers::ChangePasswordRequest,
        crate::auth::handlers::ResetPasswordRequest,
        crate::auth::model::Role,
        crate::catalog::handlers::ProductResponse,
        crate::catalog::handlers::ProductListResponse,
        crate::catalog::handlers::IdResponse,
        crate::catalog::handlers::MovementResponse,
        crate::catalog::handlers::CreateProductRequest,
        crate::catalog::handlers::UpdateProductRequest,
        crate::catalog::handlers::RecordMovementRequest,
        crate::catalog::model::ProductKind,
        crate::catalog::model::MovementReason,
        crate::customers::handlers::CustomerResponse,
        crate::customers::handlers::CustomerListResponse,
        crate::customers::handlers::CustomerIdResponse,
        crate::customers::handlers::CreateCustomerRequest,
        crate::customers::handlers::UpdateCustomerRequest,
        crate::customer_accounts::handlers::CollectReceiptRequest,
        crate::customer_accounts::handlers::ReceiptAllocationRequest,
        crate::customer_accounts::handlers::ReceiptResponse,
        crate::customer_accounts::handlers::ReceiptAllocationResponse,
        crate::customer_accounts::handlers::RefundCreditRequest,
        crate::customer_accounts::handlers::CreditRefundResponse,
        crate::customer_accounts::handlers::AdjustmentDirection,
        crate::customer_accounts::handlers::RecordAdjustmentRequest,
        crate::customer_accounts::handlers::AdjustmentResponse,
        crate::customer_accounts::handlers::AccountStatementEntryResponse,
        crate::customer_accounts::handlers::OutstandingSaleResponse,
        crate::customer_accounts::handlers::CustomerAccountResponse,
        crate::customer_accounts::handlers::ReceiptLogAllocationResponse,
        crate::customer_accounts::handlers::ReceiptLogResponse,
        crate::customer_accounts::handlers::AdjustmentLogResponse,
        crate::customer_accounts::handlers::AgingBucketsResponse,
        crate::customer_accounts::handlers::ReceivableCustomerResponse,
        crate::customer_accounts::handlers::ReceivablesResponse,
        crate::customer_accounts::model::CustomerAccountEntryKind,
        crate::sales::handlers::SaleResponse,
        crate::sales::handlers::SaleListResponse,
        crate::sales::handlers::SaleItemResponse,
        crate::sales::handlers::DraftResponse,
        crate::sales::handlers::CompletedSaleResponse,
        crate::sales::handlers::CompleteSaleRequest,
        crate::sales::handlers::SalePaymentRequest,
        crate::sales::handlers::AddItemRequest,
        crate::sales::handlers::SetQuantityRequest,
        crate::sales::handlers::SetSaleCustomerRequest,
        crate::sales::model::SaleStatus,
        crate::payments::handlers::WalletResponse,
        crate::payments::handlers::PaymentMethodResponse,
        crate::payments::handlers::WalletMovementResponse,
        crate::payments::handlers::CreateWalletRequest,
        crate::payments::handlers::UpdateWalletRequest,
        crate::payments::handlers::CreatePaymentMethodRequest,
        crate::payments::handlers::UpdatePaymentMethodRequest,
        crate::payments::handlers::ManualMovementRequest,
        crate::payments::model::WalletMovementKind,
        crate::credit_notes::handlers::CreateCreditNoteRequest,
        crate::credit_notes::handlers::CreditNoteLineRequest,
        crate::credit_notes::handlers::CreditNoteResponse,
        crate::credit_notes::handlers::CreditNoteItemResponse,
        crate::credit_notes::handlers::CreditNoteRefundResponse,
        crate::credit_notes::handlers::CreditableLineResponse,
        crate::credit_notes::handlers::SaleCreditNotesResponse,
        crate::suppliers::handlers::SupplierResponse,
        crate::suppliers::handlers::SupplierListResponse,
        crate::suppliers::handlers::SupplierIdResponse,
        crate::suppliers::handlers::CreateSupplierRequest,
        crate::suppliers::handlers::UpdateSupplierRequest,
        crate::supplier_accounts::handlers::RecordSupplierPaymentRequest,
        crate::supplier_accounts::handlers::SupplierPaymentAllocationRequest,
        crate::supplier_accounts::handlers::SupplierPaymentResponse,
        crate::supplier_accounts::handlers::SupplierPaymentAllocationResponse,
        crate::supplier_accounts::handlers::RefundSupplierCreditRequest,
        crate::supplier_accounts::handlers::SupplierCreditRefundResponse,
        crate::supplier_accounts::handlers::SupplierAdjustmentDirection,
        crate::supplier_accounts::handlers::RecordSupplierAdjustmentRequest,
        crate::supplier_accounts::handlers::SupplierAdjustmentResponse,
        crate::supplier_accounts::handlers::SupplierAccountStatementEntryResponse,
        crate::supplier_accounts::handlers::OutstandingPurchaseResponse,
        crate::supplier_accounts::handlers::SupplierAccountResponse,
        crate::supplier_accounts::handlers::SupplierPaymentLogAllocationResponse,
        crate::supplier_accounts::handlers::SupplierPaymentLogResponse,
        crate::supplier_accounts::handlers::SupplierAdjustmentLogResponse,
        crate::supplier_accounts::handlers::PayablesAgingBucketsResponse,
        crate::supplier_accounts::handlers::PayableSupplierResponse,
        crate::supplier_accounts::handlers::PayablesResponse,
        crate::supplier_accounts::model::SupplierAccountEntryKind,
        crate::purchases::handlers::PurchaseResponse,
        crate::purchases::handlers::PurchaseItemResponse,
        crate::purchases::handlers::PurchaseListResponse,
        crate::purchases::handlers::PurchaseDraftResponse,
        crate::purchases::handlers::CompletedPurchaseResponse,
        crate::purchases::handlers::SetPurchaseSupplierRequest,
        crate::purchases::handlers::AddPurchaseItemRequest,
        crate::purchases::handlers::SetPurchaseItemRequest,
        crate::purchases::handlers::PurchasePaymentRequest,
        crate::purchases::handlers::CompletePurchaseRequest,
        crate::purchases::model::PurchaseStatus,
        crate::purchase_returns::handlers::CreatePurchaseReturnRequest,
        crate::purchase_returns::handlers::PurchaseReturnLineRequest,
        crate::purchase_returns::handlers::PurchaseReturnItemResponse,
        crate::purchase_returns::handlers::PurchaseReturnRefundResponse,
        crate::purchase_returns::handlers::PurchaseReturnResponse,
        crate::purchase_returns::handlers::ReturnablePurchaseLineResponse,
        crate::purchase_returns::handlers::PurchaseReturnsSummaryResponse,
        crate::purchase_returns::model::SurplusMode,
    )),
    tags(
        (name = "auth", description = "Sessions and users"),
        (name = "catalog", description = "Products, services and stock ledger"),
        (name = "customers", description = "Customers and their documents"),
        (name = "customer_accounts", description = "Customer accounts: debt and collections"),
        (name = "sales", description = "Persisted cart and checkout"),
        (name = "payments", description = "Wallets, payment methods and the money ledger"),
        (name = "credit_notes", description = "Returns of a charged sale"),
        (name = "suppliers", description = "Suppliers and their documents"),
        (name = "supplier_accounts", description = "Supplier accounts: payables and payments"),
        (name = "purchases", description = "Persisted purchase drafts and cash supplier purchases"),
        (name = "purchase_returns", description = "Returns of a completed purchase to its supplier"),
    )
)]
pub struct ApiDoc;

/// The complete spec.
pub fn spec() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json() -> serde_json::Value {
        serde_json::to_value(spec()).expect("the spec must serialize")
    }

    /// The contract consumed by the frontend: output decimals are strings.
    #[test]
    fn decimal_outputs_are_documented_as_string() {
        let doc = json();
        let product = &doc["components"]["schemas"]["ProductResponse"]["properties"];

        for field in ["price", "stock", "min_stock"] {
            let schema = serde_json::to_string(&product[field]).unwrap();
            assert!(
                schema.contains("\"string\""),
                "ProductResponse.{field} must be documented as string, was: {schema}"
            );
            assert!(
                !schema.contains("\"number\""),
                "ProductResponse.{field} must NOT be documented as number: {schema}"
            );
        }
    }

    #[test]
    fn decimal_inputs_are_documented_as_string_too() {
        let doc = json();
        let create = &doc["components"]["schemas"]["CreateProductRequest"]["properties"];

        for field in ["price", "min_stock"] {
            let schema = serde_json::to_string(&create[field]).unwrap();
            assert!(
                schema.contains("\"string\""),
                "CreateProductRequest.{field} must be documented as string: {schema}"
            );
        }
    }

    #[test]
    fn every_documented_route_exists_under_api_v1() {
        let doc = json();
        let paths = doc["paths"].as_object().expect("paths");

        assert!(!paths.is_empty(), "the spec must not be empty");
        for path in paths.keys() {
            assert!(
                path.starts_with("/api/v1/"),
                "every documented route must live under /api/v1: {path}"
            );
        }

        // Every registered backend route.
        for expected in [
            "/api/v1/auth/login",
            "/api/v1/auth/logout",
            "/api/v1/auth/password",
            "/api/v1/auth/me",
            "/api/v1/users",
            "/api/v1/users/{id}/password",
            "/api/v1/products",
            "/api/v1/products/{id}",
            "/api/v1/products/{id}/deactivate",
            "/api/v1/products/{id}/movements",
            "/api/v1/customers",
            "/api/v1/customers/{id}",
            "/api/v1/customers/{id}/account",
            "/api/v1/customers/{id}/receipts",
            "/api/v1/customers/{id}/credit-refunds",
            "/api/v1/customers/{id}/adjustments",
            "/api/v1/accounts/receivables",
            "/api/v1/suppliers",
            "/api/v1/suppliers/{id}",
            "/api/v1/suppliers/{id}/deactivate",
            "/api/v1/suppliers/{id}/account",
            "/api/v1/suppliers/{id}/payments",
            "/api/v1/suppliers/{id}/credit-refunds",
            "/api/v1/suppliers/{id}/adjustments",
            "/api/v1/accounts/payables",
            "/api/v1/purchases/draft",
            "/api/v1/purchases",
            "/api/v1/purchases/{id}",
            "/api/v1/purchases/{id}/supplier",
            "/api/v1/purchases/{id}/items",
            "/api/v1/purchases/{id}/items/{product_id}",
            "/api/v1/purchases/{id}/complete",
            "/api/v1/purchases/{id}/cancel",
            "/api/v1/purchases/{id}/returns",
        ] {
            assert!(
                paths.contains_key(expected),
                "missing documentation for {expected}"
            );
        }
    }

    /// General sweep: NO decimal field may be documented as an object or
    /// number. `UpdateProductRequest.price` slipped in as `object`, and this
    /// test caught it, not a manual reading.
    #[test]
    fn no_decimal_field_is_documented_as_object_or_number() {
        const DECIMAL_FIELDS: &[(&str, &[&str])] = &[
            ("ProductResponse", &["price", "stock", "min_stock"]),
            (
                "MovementResponse",
                &["quantity", "signed_quantity", "unit_cost"],
            ),
            ("CreateProductRequest", &["price", "min_stock"]),
            ("UpdateProductRequest", &["price", "min_stock"]),
            ("RecordMovementRequest", &["quantity", "unit_cost"]),
            (
                "CustomerResponse",
                &["balance", "available_credit", "credit_limit"],
            ),
            (
                "CustomerAccountResponse",
                &["balance", "unapplied_credit", "outstanding_total"],
            ),
            (
                "AccountStatementEntryResponse",
                &["amount", "running_balance"],
            ),
            (
                "OutstandingSaleResponse",
                &["charged", "cancelled", "allocated", "outstanding"],
            ),
            ("ReceiptLogResponse", &["amount", "allocated_total"]),
            ("ReceiptLogAllocationResponse", &["amount"]),
            ("AdjustmentLogResponse", &["amount"]),
            (
                "AgingBucketsResponse",
                &["days_0_30", "days_31_60", "days_61_90", "days_90_plus"],
            ),
            (
                "ReceivableCustomerResponse",
                &[
                    "balance",
                    "unapplied_credit",
                    "outstanding_total",
                    "adjustments",
                ],
            ),
            (
                "ReceivablesResponse",
                &[
                    "total_balance",
                    "total_unapplied_credit",
                    "total_outstanding",
                    "total_adjustments",
                ],
            ),
            (
                "SupplierResponse",
                &["balance", "available_credit", "credit_limit"],
            ),
            ("CreateSupplierRequest", &["credit_limit"]),
            ("UpdateSupplierRequest", &["credit_limit"]),
            (
                "SupplierAccountResponse",
                &["balance", "unapplied_credit", "outstanding_total"],
            ),
            (
                "SupplierAccountStatementEntryResponse",
                &["amount", "running_balance"],
            ),
            (
                "OutstandingPurchaseResponse",
                &["charged", "cancelled", "allocated", "outstanding"],
            ),
            ("RecordSupplierPaymentRequest", &["amount"]),
            ("SupplierPaymentAllocationRequest", &["amount"]),
            ("SupplierPaymentResponse", &["balance", "unapplied_credit"]),
            ("SupplierPaymentAllocationResponse", &["outstanding"]),
            ("SupplierPaymentLogResponse", &["amount", "allocated_total"]),
            ("SupplierPaymentLogAllocationResponse", &["amount"]),
            ("RefundSupplierCreditRequest", &["amount"]),
            (
                "SupplierCreditRefundResponse",
                &["balance", "unapplied_credit"],
            ),
            ("RecordSupplierAdjustmentRequest", &["amount"]),
            (
                "SupplierAdjustmentResponse",
                &["balance", "unapplied_credit"],
            ),
            ("SupplierAdjustmentLogResponse", &["amount"]),
            (
                "PayablesAgingBucketsResponse",
                &["days_0_30", "days_31_60", "days_61_90", "days_90_plus"],
            ),
            ("PurchaseReturnLineRequest", &["quantity"]),
            (
                "PurchaseReturnItemResponse",
                &["quantity", "unit_cost", "subtotal"],
            ),
            ("PurchaseReturnRefundResponse", &["amount"]),
            (
                "PurchaseReturnResponse",
                &["total", "debt_cancelled", "credit_granted"],
            ),
            (
                "ReturnablePurchaseLineResponse",
                &[
                    "quantity",
                    "unit_cost",
                    "returned_quantity",
                    "returnable_quantity",
                ],
            ),
            (
                "PurchaseReturnsSummaryResponse",
                &[
                    "purchase_total",
                    "returned_total",
                    "returnable_total",
                    "purchase_outstanding",
                    "wallet_refundable_capacity",
                ],
            ),
            (
                "PayableSupplierResponse",
                &[
                    "balance",
                    "unapplied_credit",
                    "outstanding_total",
                    "adjustments",
                ],
            ),
            (
                "PayablesResponse",
                &[
                    "total_balance",
                    "total_unapplied_credit",
                    "total_outstanding",
                    "total_adjustments",
                ],
            ),
            ("PurchaseResponse", &["total"]),
            (
                "PurchaseItemResponse",
                &["quantity", "unit_cost", "subtotal"],
            ),
            ("CompletedPurchaseResponse", &["total", "credit_applied"]),
            ("CompletePurchaseRequest", &["on_account"]),
            ("AddPurchaseItemRequest", &["quantity", "unit_cost"]),
            ("SetPurchaseItemRequest", &["quantity", "unit_cost"]),
            ("PurchasePaymentRequest", &["amount"]),
        ];

        let doc = json();
        let schemas = &doc["components"]["schemas"];

        for (schema_name, fields) in DECIMAL_FIELDS {
            for field in *fields {
                let raw = serde_json::to_string(&schemas[schema_name]["properties"][field])
                    .unwrap_or_else(|_| panic!("{schema_name}.{field} missing"));

                assert!(
                    !raw.contains("\"number\""),
                    "{schema_name}.{field} must not be number: {raw}"
                );
                assert!(
                    !raw.contains("\"object\""),
                    "{schema_name}.{field} must not be object: {raw}"
                );
                assert!(
                    raw.contains("\"string\""),
                    "{schema_name}.{field} must be string: {raw}"
                );
            }
        }
    }

    #[test]
    fn problem_schema_matches_rfc7807() {
        let doc = json();
        let problem = &doc["components"]["schemas"]["Problem"]["properties"];

        for field in ["type", "title", "status", "detail"] {
            assert!(
                !problem[field].is_null(),
                "Problem must declare field {field}"
            );
        }
    }
}
