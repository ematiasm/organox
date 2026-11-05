//! Per-IP rate-limiting middleware.
//!
//! Applied only to sensitive routes (login), not the whole API: limiting
//! `GET /products` would affect a legitimate polling frontend and would not
//! help against brute force.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::ratelimit::client_ip;

pub async fn limit_by_ip(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // `ConnectInfo` is the TCP connection's IP. If unavailable (tests without
    // `into_make_service_with_connect_info`), `client_ip` falls back to loopback.
    //
    // DO NOT read `X-Forwarded-For`: the client controls this header, and
    // honoring it without a trusted proxy allows choosing the IP on each
    // request and bypassing the limit.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());

    let ip = client_ip(peer);

    if let Err(retry_after) = state.ip_limiter.check_and_record(ip) {
        tracing::warn!(%ip, retry_after, "IP rate limit exceeded");

        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "type": "about:blank",
                "title": "Too Many Requests",
                "status": 429,
                "detail": "Too many attempts from this address. Wait a few seconds.",
            })),
        )
            .into_response();

        if let Ok(value) = retry_after.to_string().parse() {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        return response;
    }

    next.run(request).await
}
