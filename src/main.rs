//! Entry point. Sole responsibility: load config, initialize logging, connect
//! the DB, assemble the router and serve. No business logic.

use std::process::ExitCode;

use organox::config::Config;
use organox::telemetry;
use organox::{AppState, db, routes};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The logger may not yet be initialized, so also write to stderr.
            eprintln!("fatal error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    telemetry::init(config.log_format, &config.rust_log);

    // Warm the dummy hash outside the request path: if calculated on the
    // first failed login, that extra latency would distinguish "unknown email"
    // from "incorrect password".
    tokio::task::spawn_blocking(organox::auth::service::warm_dummy_hash)
        .await
        .ok();

    let pool = db::connect(&config.database_url).await?;
    tracing::info!(
        bind = %config.bind_addr,
        db_max_connections = pool.options().get_max_connections(),
        "starting organox"
    );

    // CookieManagerLayer must wrap the router: the `Cookies` and `AuthUser`
    // extractors depend on it.
    let state = AppState::with_policies(
        pool,
        config.cookie_secure,
        config.account_policy(),
        config.ip_policy(),
    );
    let app = routes::router(state)
        .layer(tower_cookies::CookieManagerLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        // Generate the request id BEFORE the trace span so the span sees it,
        // and propagate it in the response.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);

    // `into_make_service_with_connect_info` lets rate limiting middleware see
    // the connection IP. Without it, all requests fall under loopback and the
    // per-IP limit would be useless in production.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

/// Wait for SIGINT or SIGTERM to shut down gracefully (stop accepting
/// connections and finish in-flight requests).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install the Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, shutting down");
}
