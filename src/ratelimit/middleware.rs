//! Middleware de límite por IP.
//!
//! Se aplica solo a las rutas sensibles (login), no a toda la API: limitar
//! `GET /products` afectaría a un frontend legítimo que hace polling, y no
//! aporta contra fuerza bruta.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::ratelimit::client_ip;

pub async fn limit_by_ip(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // `ConnectInfo` es la IP de la conexión TCP. Si no está disponible (tests
    // sin `into_make_service_with_connect_info`), `client_ip` cae a loopback.
    //
    // NO se lee `X-Forwarded-For`: es un header que el cliente controla, y
    // honrarlo sin un proxy confiable delante permite elegir la IP en cada
    // request y anular el límite.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());

    let ip = client_ip(peer);

    if let Err(retry_after) = state.ip_limiter.check_and_record(ip) {
        tracing::warn!(%ip, retry_after, "rate limit por IP excedido");

        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "type": "about:blank",
                "title": "Too Many Requests",
                "status": 429,
                "detail": "Demasiados intentos desde esta dirección. Esperá unos segundos.",
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
