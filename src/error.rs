//! One error type for the entire application.
//!
//! Every error crossing the HTTP boundary is converted here, with a consistent
//! body (`application/problem+json`, RFC 7807). Internal details are logged,
//! never serialized to the client.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid credentials")]
    Unauthorized,

    #[error("permission denied")]
    Forbidden,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("resource not found")]
    NotFound,

    /// Too many requests or a locked account. `retry_after_secs` is sent in
    /// the `Retry-After` header, the standard mechanism (RFC 9110) for a
    /// client to know when to retry without guessing.
    #[error("too many attempts")]
    TooManyRequests { retry_after_secs: u64 },

    #[error("invalid input: {0}")]
    Validation(String),

    #[error("database unavailable: {0}")]
    DatabaseUnavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, detail) = match &self {
            AppError::Config(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Configuration Error",
                "The server is misconfigured.".to_string(),
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Unauthorized",
                "Invalid credentials or expired session.".to_string(),
            ),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                "Forbidden",
                "You do not have permission to perform this operation.".to_string(),
            ),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "Conflict", msg.clone()),
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                "Not Found",
                "The requested resource was not found.".to_string(),
            ),
            AppError::TooManyRequests { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too Many Requests",
                "Too many attempts. Wait a few minutes before retrying.".to_string(),
            ),
            AppError::Validation(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Validation Error",
                msg.clone(),
            ),
            // Do not leak the sqlx message to the client: it may contain a host,
            // user or infrastructure details.
            AppError::DatabaseUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable",
                "The database is not responding.".to_string(),
            ),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "An unexpected error occurred.".to_string(),
            ),
        };

        // Log 5xx as errors (with details); log 4xx as info (the client made
        // a mistake, not the server) without internal details.
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

        // RFC 7807 requires the Content-Type `application/problem+json`; without
        // it, a client cannot distinguish an error body from a normal response,
        // and errors are parsed as data. This was documented before it was
        // implemented: the OpenAPI contract test caught this bug.
        let mut response = (
            status,
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/problem+json"),
            )],
            Json(body),
        )
            .into_response();

        // `Retry-After` makes a 429 actionable: the client knows when to
        // retry instead of hammering the server.
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

#[cfg(test)]
mod translation_tests {
    use super::*;

    #[tokio::test]
    async fn configuration_error_detail_is_english() {
        let response = AppError::Config("private".into()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "application/problem+json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["detail"], "The server is misconfigured.");
        assert_eq!(body["title"], "Configuration Error");
        assert_eq!(body["status"], 500);
        assert_eq!(body["type"], "about:blank");
    }
}
