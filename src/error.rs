//! Un único tipo de error para toda la aplicación.
//!
//! Todo error que cruce el borde HTTP se convierte acá, con un cuerpo
//! homogéneo (`application/problem+json`, RFC 7807). Los detalles internos se
//! loguean, nunca se serializan al cliente.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("error de configuración: {0}")]
    Config(String),

    #[error("credenciales inválidas")]
    Unauthorized,

    #[error("sin permiso")]
    Forbidden,

    #[error("conflicto: {0}")]
    Conflict(String),

    #[error("no se encontró el recurso")]
    NotFound,

    /// Demasiadas requests o cuenta bloqueada. `retry_after_secs` se envía en
    /// el header `Retry-After`, que es el mecanismo estándar (RFC 9110) para
    /// que un cliente sepa cuándo reintentar sin adivinar.
    #[error("demasiados intentos")]
    TooManyRequests { retry_after_secs: u64 },

    #[error("entrada inválida: {0}")]
    Validation(String),

    #[error("la base de datos no está disponible: {0}")]
    DatabaseUnavailable(String),

    #[error("error interno: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, detail) = match &self {
            AppError::Config(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Configuration Error",
                "El servidor está mal configurado.".to_string(),
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Unauthorized",
                "Credenciales inválidas o sesión vencida.".to_string(),
            ),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                "Forbidden",
                "No tenés permiso para esta operación.".to_string(),
            ),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "Conflict", msg.clone()),
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                "Not Found",
                "No se encontró el recurso solicitado.".to_string(),
            ),
            AppError::TooManyRequests { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too Many Requests",
                "Demasiados intentos. Esperá unos minutos antes de reintentar.".to_string(),
            ),
            AppError::Validation(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Validation Error",
                msg.clone(),
            ),
            // No filtramos el mensaje de sqlx al cliente: puede contener host,
            // usuario o detalles de infraestructura.
            AppError::DatabaseUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable",
                "La base de datos no responde.".to_string(),
            ),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "Ocurrió un error inesperado.".to_string(),
            ),
        };

        // Los 5xx se loguean como error (con detalle); los 4xx como info (el
        // cliente se equivocó, no el servidor) y sin el detalle interno.
        if status.is_server_error() {
            tracing::error!(error = %self, status = status.as_u16(), "request failed");
        } else {
            tracing::info!(status = status.as_u16(), "request rejected");
        }

        let body = json!({
            "type": "about:blank",
            "title": title,
            "status": status.as_u16(),
            "detail": detail,
        });

        // RFC 7807 exige el Content-Type `application/problem+json`; sin él, un
        // cliente no puede distinguir un cuerpo de error de una respuesta
        // normal y los errores se parsean como si fueran datos. Se documentó
        // antes de implementarse: este fue el bug que corrigió el test de
        // contrato del OpenAPI.
        let mut response = (
            status,
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/problem+json"),
            )],
            Json(body),
        )
            .into_response();

        // `Retry-After` es lo que convierte un 429 en algo accionable: el
        // cliente sabe cuándo volver a intentar en vez de martillar.
        if let AppError::TooManyRequests { retry_after_secs } = &self
            && let Ok(value) = retry_after_secs.to_string().parse()
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }

        response
    }
}
