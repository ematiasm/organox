//! Entry point. Sole responsibility: load config, initialize logging, connect
//! the DB, assemble the router and serve. No business logic.

use std::process::ExitCode;

use organox::config::Config;
use organox::telemetry;
use organox::{AppState, db, routes};

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

    let state = AppState::with_policies(
        pool,
        config.cookie_secure,
        config.account_policy(),
        config.ip_policy(),
    )
    .with_docs(config.enable_docs);

    // Periodically purge expired sessions and stale login_attempts in the
    // background, stopping cleanly when the server receives SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let cleanup_handle = organox::auth::cleanup::spawn(
        state.pool.clone(),
        state.account_limiter.clone(),
        organox::auth::cleanup::DEFAULT_CLEANUP_INTERVAL,
        organox::auth::cleanup::attempt_retention(state.account_limiter.policy()),
        shutdown_rx,
    );

    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);

    // `into_make_service_with_connect_info` lets rate limiting middleware see
    // the connection IP. Without it, all requests fall under loopback and the
    // per-IP limit would be useless in production.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    })
    .await?;

    let _ = cleanup_handle.await;

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
