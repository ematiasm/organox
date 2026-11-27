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

    let listener = match tokio::net::TcpListener::bind(config.bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            pool.close().await;
            return Err(err.into());
        }
    };
    let local_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            pool.close().await;
            return Err(err.into());
        }
    };
    tracing::info!("listening on http://{local_addr}");

    let state = AppState::with_policies(
        pool,
        config.cookie_secure,
        config.account_policy(),
        config.ip_policy(),
    )
    .with_docs(config.enable_docs);

    serve_until_shutdown(
        listener,
        state,
        organox::auth::cleanup::DEFAULT_CLEANUP_INTERVAL,
        shutdown_signal(),
    )
    .await?;

    Ok(())
}

async fn serve_until_shutdown<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    cleanup_interval: std::time::Duration,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let pool = state.pool.clone();

    // Periodically purge expired sessions and stale login_attempts in the
    // background, stopping cleanly when the server receives SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = std::sync::Arc::new(shutdown_tx);
    let cleanup_handle = organox::auth::cleanup::spawn(
        state.pool.clone(),
        state.account_limiter.clone(),
        cleanup_interval,
        organox::auth::cleanup::attempt_retention(state.account_limiter.policy()),
        shutdown_rx,
    );

    let app = routes::router(state);

    // `into_make_service_with_connect_info` lets rate limiting middleware see
    // the connection IP. Without it, all requests fall under loopback and the
    // per-IP limit would be useless in production.
    let signal_tx = std::sync::Arc::clone(&shutdown_tx);
    let serve_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        let _ = signal_tx.send(true);
    })
    .await;

    // Signal cleanup even if `axum::serve` exited with an error before
    // `shutdown` resolved, wait for any in-flight purge pass to finish, then
    // drain and close the Postgres pool.
    let _ = shutdown_tx.send(true);
    if let Err(err) = cleanup_handle.await {
        tracing::warn!(error = %err, "cleanup task failed");
    }
    pool.close().await;
    tracing::info!("database pool closed");

    serve_result
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[tokio::test]
    async fn serve_until_shutdown_closes_db_pool_on_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/never_connected")
            .expect("lazy pool");
        let state = AppState::new(pool.clone(), false);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_shutdown(
            listener,
            state,
            Duration::from_secs(3600),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        let response = reqwest::get(format!("http://{addr}/healthz"))
            .await
            .expect("healthz request succeeds while running");
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        assert!(
            !pool.is_closed(),
            "DB pool must remain open while server is serving requests"
        );

        shutdown_tx.send(()).expect("trigger shutdown");

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server shuts down promptly")
            .expect("server task does not panic")
            .expect("axum::serve exits cleanly");

        assert!(
            pool.is_closed(),
            "DB pool must be closed during graceful shutdown"
        );
    }

    #[tokio::test]
    async fn serve_until_shutdown_drains_idle_keepalive_and_closes_pool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/never_connected")
            .expect("lazy pool");
        let state = AppState::new(pool.clone(), false);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_shutdown(
            listener,
            state,
            Duration::from_secs(3600),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Keep an HTTP/1.1 keep-alive client alive across shutdown to prove
        // idle connections do not hang graceful shutdown or pool closure.
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("healthz over keep-alive client");
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);

        shutdown_tx.send(()).expect("trigger shutdown");

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("idle keep-alive connection must not block shutdown")
            .expect("server task does not panic")
            .expect("axum::serve exits cleanly");

        drop(client);
        assert!(pool.is_closed());
    }

    #[tokio::test]
    async fn serve_until_shutdown_closes_pool_when_shutdown_is_already_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/never_connected")
            .expect("lazy pool");
        let state = AppState::new(pool.clone(), false);

        tokio::time::timeout(
            Duration::from_secs(5),
            serve_until_shutdown(
                listener,
                state,
                Duration::from_secs(3600),
                std::future::ready(()),
            ),
        )
        .await
        .expect("immediate shutdown must not hang")
        .expect("axum::serve exits cleanly");

        assert!(pool.is_closed());
    }
}
