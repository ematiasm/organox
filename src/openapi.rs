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
        crate::auth::handlers::login_doc,
        crate::auth::handlers::logout_doc,
        crate::auth::handlers::me_doc,
        crate::auth::handlers::create_user_doc,
        crate::catalog::handlers::list_products_doc,
        crate::catalog::handlers::create_product_doc,
        crate::catalog::handlers::get_product_doc,
        crate::catalog::handlers::update_product_doc,
        crate::catalog::handlers::deactivate_product_doc,
        crate::catalog::handlers::list_movements_doc,
        crate::catalog::handlers::record_movement_doc,
        crate::sales::handlers::open_draft_doc,
        crate::sales::handlers::get_sale_doc,
        crate::sales::handlers::add_item_doc,
        crate::sales::handlers::set_quantity_doc,
        crate::sales::handlers::remove_item_doc,
        crate::sales::handlers::complete_sale_doc,
        crate::sales::handlers::cancel_sale_doc,
        crate::sales::handlers::list_sales_doc,
    ),
    components(schemas(
        Problem,
        crate::auth::handlers::LoginRequest,
        crate::auth::handlers::UserResponse,
        crate::auth::handlers::LoginResponse,
        crate::auth::handlers::CreateUserRequest,
        crate::auth::handlers::CreateUserResponse,
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
        crate::sales::handlers::SaleResponse,
        crate::sales::handlers::SaleItemResponse,
        crate::sales::handlers::DraftResponse,
        crate::sales::handlers::CompletedSaleResponse,
        crate::sales::handlers::AddItemRequest,
        crate::sales::handlers::SetQuantityRequest,
        crate::sales::model::SaleStatus,
    )),
    tags(
        (name = "auth", description = "Sessions and users"),
        (name = "catalog", description = "Products, services and stock ledger"),
        (name = "sales", description = "Persisted cart and checkout"),
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

        // The eleven real backend routes.
        for expected in [
            "/api/v1/auth/login",
            "/api/v1/auth/logout",
            "/api/v1/auth/me",
            "/api/v1/users",
            "/api/v1/products",
            "/api/v1/products/{id}",
            "/api/v1/products/{id}/deactivate",
            "/api/v1/products/{id}/movements",
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
