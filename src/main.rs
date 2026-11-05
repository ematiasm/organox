//! Punto de entrada. Responsabilidad única: cargar config, inicializar logging,
//! conectar la DB, armar el router y servir. Sin lógica de negocio.

use std::process::ExitCode;

use admin_app::config::Config;
use admin_app::telemetry;
use admin_app::{AppState, db, routes};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // En este punto el logger puede no estar inicializado, por eso
            // también escribimos a stderr.
            eprintln!("error fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    telemetry::init(config.log_format, &config.rust_log);

    // Precalentar el hash señuelo fuera del camino de una request: si se
    // calculara en el primer login fallido, esa latencia extra distinguiría
    // "email inexistente" de "contraseña incorrecta".
    tokio::task::spawn_blocking(admin_app::auth::service::warm_dummy_hash)
        .await
        .ok();

    let pool = db::connect(&config.database_url).await?;
    tracing::info!(
        bind = %config.bind_addr,
        db_max_connections = pool.options().get_max_connections(),
        "iniciando admin-app"
    );

    // El CookieManagerLayer tiene que envolver al router: los extractores
    // `Cookies` y `AuthUser` dependen de él.
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
        // El request id debe generarse ANTES del span de trace para que el
        // span ya lo vea, y propagarse en la respuesta.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!("escuchando en http://{}", listener.local_addr()?);

    // `into_make_service_with_connect_info` es lo que permite al middleware de
    // rate limiting ver la IP de la conexión. Sin esto, todas las requests
    // caen bajo loopback y el límite por IP sería inútil en producción.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

/// Espera SIGINT o SIGTERM para cerrar ordenadamente (deja de aceptar
/// conexiones y termina las requests en curso).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("no se pudo instalar el handler de Ctrl+C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("no se pudo instalar el handler de SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("señal de apagado recibida, cerrando");
}
