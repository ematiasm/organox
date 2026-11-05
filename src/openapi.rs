//! Especificación OpenAPI 3.1 generada desde el código.
//!
//! # Por qué existe
//!
//! El frontend consume tipos de TypeScript **generados** desde este spec, no
//! escritos a mano. Si el backend cambia un campo, el `schema.d.ts` cambia y
//! `svelte-check` falla. Sin esto, el contrato vive en la cabeza de quien
//! escribió el handler.
//!
//! # La regla que el spec debe respetar
//!
//! Los decimales son **string** en ambos sentidos:
//!
//! - **Salida**: `price`, `stock`, `min_stock`, `quantity`, `unit_cost` son
//!   `String` en los DTOs. Un `f64` en JSON pierde precisión en dinero.
//! - **Entrada**: el servidor *también* acepta números (`rust_decimal` los
//!   deserializa), pero el spec documenta **solo string** porque es el contrato
//!   que el frontend debe usar. Aceptar números es tolerancia del parseo, no
//!   parte del contrato.
//!   Ver `tests/catalog.rs::decimal_inputs_accept_both_string_and_number`.
//!
//! # Errores
//!
//! Todas las respuestas de error usan `application/problem+json` (RFC 7807) con
//! el schema [`Problem`]. El `429` agrega el header `Retry-After`.

use utoipa::OpenApi;

/// Cuerpo de error RFC 7807. Mismo shape para todos los códigos.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub struct Problem {
    /// URI que identifica el tipo de problema. Siempre `about:blank` hoy.
    #[schema(example = "about:blank")]
    pub r#type: String,
    /// Resumen legible y estable del problema.
    #[schema(example = "Unauthorized")]
    pub title: String,
    /// Código HTTP, repetido en el cuerpo para clientes que no leen headers.
    #[schema(example = 401)]
    pub status: u16,
    /// Explicación accionable para el usuario.
    #[schema(example = "Credenciales inválidas o sesión vencida.")]
    pub detail: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "organox",
        version = env!("CARGO_PKG_VERSION"),
        description = "Administrador de negocio para una PyME (stock e insumos). \
                       Single-tenant.\n\n\
                       **Autenticación**: salvo `POST /api/v1/auth/login`, toda ruta \
                       requiere la cookie de sesión `session` (HttpOnly, SameSite=Strict). \
                       Sin ella devuelve `401`.\n\n\
                       **Errores**: todas las respuestas de error usan \
                       `application/problem+json` (RFC 7807). El `429` incluye además \
                       el header `Retry-After` en segundos.\n\n\
                       **Decimales**: viajan siempre como **string** (`\"1500.50\"`, \
                       `\"12.5\"`), nunca como número, para no perder precisión."
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
        (name = "auth", description = "Sesión y usuarios"),
        (name = "catalog", description = "Productos, servicios y libro de stock"),
        (name = "sales", description = "Carrito persistido y cobro"),
    )
)]
pub struct ApiDoc;

/// El spec completo.
pub fn spec() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json() -> serde_json::Value {
        serde_json::to_value(spec()).expect("el spec debe serializar")
    }

    /// El contrato que el frontend consume: los decimales de salida son string.
    #[test]
    fn decimal_outputs_are_documented_as_string() {
        let doc = json();
        let product = &doc["components"]["schemas"]["ProductResponse"]["properties"];

        for field in ["price", "stock", "min_stock"] {
            let schema = serde_json::to_string(&product[field]).unwrap();
            assert!(
                schema.contains("\"string\""),
                "ProductResponse.{field} debe documentarse como string, fue: {schema}"
            );
            assert!(
                !schema.contains("\"number\""),
                "ProductResponse.{field} NO debe documentarse como número: {schema}"
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
                "CreateProductRequest.{field} debe documentarse como string: {schema}"
            );
        }
    }

    #[test]
    fn every_documented_route_exists_under_api_v1() {
        let doc = json();
        let paths = doc["paths"].as_object().expect("paths");

        assert!(!paths.is_empty(), "el spec no debe estar vacío");
        for path in paths.keys() {
            assert!(
                path.starts_with("/api/v1/"),
                "toda ruta documentada debe vivir bajo /api/v1: {path}"
            );
        }

        // Las once rutas reales del backend.
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
            assert!(paths.contains_key(expected), "falta documentar {expected}");
        }
    }

    /// Barrido general: NINGÚN campo decimal puede documentarse como objeto o
    /// número. El caso de `UpdateProductRequest.price` se coló como `object`
    /// y lo detectó este test, no una lectura.
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
                    .unwrap_or_else(|_| panic!("{schema_name}.{field} ausente"));

                assert!(
                    !raw.contains("\"number\""),
                    "{schema_name}.{field} no debe ser número: {raw}"
                );
                assert!(
                    !raw.contains("\"object\""),
                    "{schema_name}.{field} no debe ser object: {raw}"
                );
                assert!(
                    raw.contains("\"string\""),
                    "{schema_name}.{field} debe ser string: {raw}"
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
                "Problem debe declarar el campo {field}"
            );
        }
    }
}
