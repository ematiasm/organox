//! OpenAPI contract tests for supplier accounts, suppliers, purchases, and
//! wallet movement kinds.

use serde_json::Value;

fn spec_json() -> Value {
    serde_json::to_value(organox::openapi::spec()).expect("OpenAPI spec must serialize")
}

#[test]
fn supplier_accounts_paths_and_tag_are_documented() {
    let doc = spec_json();
    let paths = doc["paths"].as_object().expect("paths object");

    for (path, methods) in [
        ("/api/v1/suppliers/{id}/account", &["get"][..]),
        ("/api/v1/suppliers/{id}/payments", &["get", "post"][..]),
        ("/api/v1/suppliers/{id}/credit-refunds", &["post"][..]),
        ("/api/v1/suppliers/{id}/adjustments", &["get", "post"][..]),
        ("/api/v1/accounts/payables", &["get"][..]),
        ("/api/v1/purchases/{id}/returns", &["get", "post"][..]),
    ] {
        let entry = paths
            .get(path)
            .unwrap_or_else(|| panic!("missing documented path {path}"));
        for method in methods {
            assert!(
                entry.get(*method).is_some(),
                "missing documented method {method} on {path}"
            );
        }
    }

    let tags = doc["tags"].as_array().expect("tags array");
    assert!(
        tags.iter().any(|t| t["name"] == "supplier_accounts"),
        "missing supplier_accounts tag"
    );
    assert!(
        tags.iter().any(|t| t["name"] == "purchase_returns"),
        "missing purchase_returns tag"
    );
}

#[test]
fn supplier_accounts_suppliers_purchases_and_wallet_schemas_are_complete() {
    let doc = spec_json();
    let schemas = &doc["components"]["schemas"];

    // SupplierResponse includes credit_limit, balance, available_credit as strings.
    for field in ["credit_limit", "balance", "available_credit"] {
        let raw = serde_json::to_string(&schemas["SupplierResponse"]["properties"][field])
            .unwrap_or_else(|_| panic!("SupplierResponse.{field} missing"));
        assert!(
            raw.contains("\"string\""),
            "SupplierResponse.{field}: {raw}"
        );
        assert!(
            !raw.contains("\"number\""),
            "SupplierResponse.{field}: {raw}"
        );
    }

    // Purchases on_account and credit_applied are documented as strings.
    for (schema_name, prop_name) in [
        ("CompletePurchaseRequest", "on_account"),
        ("CompletedPurchaseResponse", "credit_applied"),
    ] {
        let raw = serde_json::to_string(&schemas[schema_name]["properties"][prop_name])
            .unwrap_or_else(|_| panic!("{schema_name}.{prop_name} missing"));
        assert!(
            raw.contains("\"string\""),
            "{schema_name}.{prop_name}: {raw}"
        );
    }

    // WalletMovementKind includes the two new supplier account kinds.
    let kinds = schemas["WalletMovementKind"]["enum"]
        .as_array()
        .expect("WalletMovementKind enum");
    assert!(
        kinds.iter().any(|v| v == "purchase_refund"),
        "missing purchase_refund in WalletMovementKind"
    );
    assert!(
        kinds.iter().any(|v| v == "supplier_account_payment"),
        "missing supplier_account_payment in WalletMovementKind"
    );
    assert!(
        kinds.iter().any(|v| v == "supplier_account_refund"),
        "missing supplier_account_refund in WalletMovementKind"
    );

    // All supplier account DTO decimal fields are documented as strings.
    const DECIMAL_FIELDS: &[(&str, &[&str])] = &[
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
    ];

    for (schema_name, fields) in DECIMAL_FIELDS {
        for field in *fields {
            let prop = &schemas[schema_name]["properties"][field];
            assert!(
                !prop.is_null(),
                "{schema_name}.{field} missing from OpenAPI schemas"
            );
            let raw = serde_json::to_string(prop).unwrap();
            assert!(
                raw.contains("\"string\"")
                    && !raw.contains("\"number\"")
                    && !raw.contains("\"object\""),
                "{schema_name}.{field} must be string, was: {raw}"
            );
        }
    }
}
