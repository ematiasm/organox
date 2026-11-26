//! Thin Axum extractor wrappers that convert rejection responses into RFC 7807
//! `application/problem+json` via [`AppError`].
//!
//! Raw `axum::Json`, `axum::extract::Path`, and `axum::extract::Query` answer
//! malformed payloads, wrong types, missing `Content-Type`, invalid UUIDs, or
//! malformed query strings with `text/plain`. Wrapping them here keeps every
//! error across the HTTP boundary consistent with the documented contract.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;

use crate::error::AppError;

/// JSON request body extractor returning RFC 7807 [`AppError`] on rejection.
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_json_rejection(rejection)),
        }
    }
}

/// Path parameter extractor returning RFC 7807 [`AppError`] on rejection.
pub struct AppPath<T>(pub T);

impl<S, T> FromRequestParts<S> for AppPath<T>
where
    axum::extract::Path<T>: FromRequestParts<S, Rejection = PathRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_path_rejection(rejection)),
        }
    }
}

/// Query string extractor returning RFC 7807 [`AppError`] on rejection.
pub struct AppQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    axum::extract::Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_query_rejection(rejection)),
        }
    }
}

fn map_json_rejection(rejection: JsonRejection) -> AppError {
    match rejection {
        JsonRejection::JsonDataError(err) => AppError::Validation(err.body_text()),
        JsonRejection::JsonSyntaxError(err) => AppError::BadRequest(err.body_text()),
        JsonRejection::MissingJsonContentType(err) => {
            AppError::UnsupportedMediaType(err.body_text())
        }
        JsonRejection::BytesRejection(err) => match err.status() {
            StatusCode::PAYLOAD_TOO_LARGE => AppError::PayloadTooLarge,
            s if s.is_server_error() => AppError::Internal(err.to_string()),
            _ => AppError::BadRequest(err.body_text()),
        },
        other => match other.status() {
            StatusCode::PAYLOAD_TOO_LARGE => AppError::PayloadTooLarge,
            StatusCode::UNSUPPORTED_MEDIA_TYPE => AppError::UnsupportedMediaType(other.body_text()),
            StatusCode::UNPROCESSABLE_ENTITY => AppError::Validation(other.body_text()),
            s if s.is_server_error() => AppError::Internal(other.to_string()),
            _ => AppError::BadRequest(other.body_text()),
        },
    }
}

fn map_path_rejection(rejection: PathRejection) -> AppError {
    match rejection {
        PathRejection::FailedToDeserializePathParams(err) => AppError::BadRequest(err.body_text()),
        PathRejection::MissingPathParams(err) => AppError::Internal(err.to_string()),
        other => {
            if other.status().is_server_error() {
                AppError::Internal(other.to_string())
            } else {
                AppError::BadRequest(other.body_text())
            }
        }
    }
}

fn map_query_rejection(rejection: QueryRejection) -> AppError {
    match rejection {
        QueryRejection::FailedToDeserializeQueryString(err) => {
            AppError::BadRequest(err.body_text())
        }
        other => {
            if other.status().is_server_error() {
                AppError::Internal(other.to_string())
            } else {
                AppError::BadRequest(other.body_text())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use serde::Deserialize;
    use serde_json::Value;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;

    #[derive(Deserialize)]
    struct SampleBody {
        #[allow(dead_code)]
        name: String,
    }

    #[derive(Deserialize)]
    struct SampleQuery {
        #[allow(dead_code)]
        limit: i64,
    }

    async fn assert_problem(response: axum::response::Response, expected: StatusCode) {
        assert_eq!(response.status(), expected);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "about:blank");
        assert_eq!(body["status"], expected.as_u16());
        assert!(body["title"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(body["detail"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn app_json_maps_syntax_data_and_media_type_rejections_to_problem_json() {
        let app = Router::new().route(
            "/test",
            post(|AppJson(_): AppJson<SampleBody>| async { StatusCode::OK }),
        );

        let syntax = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{bad"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem(syntax, StatusCode::BAD_REQUEST).await;

        let data = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":123}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem(data, StatusCode::UNPROCESSABLE_ENTITY).await;

        let missing_ct = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/test")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(r#"{"name":"ok"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem(missing_ct, StatusCode::UNSUPPORTED_MEDIA_TYPE).await;
    }

    #[tokio::test]
    async fn app_path_and_app_query_map_rejections_to_problem_json() {
        let app = Router::new()
            .route(
                "/items/{id}",
                get(|AppPath(_): AppPath<Uuid>| async { StatusCode::OK }),
            )
            .route(
                "/items",
                get(|AppQuery(_): AppQuery<SampleQuery>| async { StatusCode::OK }),
            );

        let bad_path = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/items/not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem(bad_path, StatusCode::BAD_REQUEST).await;

        let bad_query = app
            .oneshot(
                Request::builder()
                    .uri("/items?limit=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_problem(bad_query, StatusCode::BAD_REQUEST).await;
    }
}
