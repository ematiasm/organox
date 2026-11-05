//! Tests de configuración.
//!
//! Un solo test deliberado: las variables de entorno son estado global del
//! proceso, y varios tests que las mutan en paralelo se pisan entre sí.

use admin_app::config::{Config, LogFormat};

/// Fija variables de entorno, corre `f` y restaura los valores previos.
fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let keys: Vec<String> = vars.iter().map(|(k, _)| k.to_string()).collect();
    let previous: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();

    for (key, value) in vars {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    f();

    for (key, value) in keys.iter().zip(previous) {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

#[test]
fn config_validates_environment() {
    // 1. Sin DATABASE_URL: error explícito, no arranca.
    with_env(&[("DATABASE_URL", None)], || {
        let err = Config::from_env().expect_err("debe fallar sin DATABASE_URL");
        assert!(
            err.to_string().contains("DATABASE_URL"),
            "el error debe nombrar la variable faltante, fue: {err}"
        );
    });

    // 2. Valores por defecto cuando solo está la URL obligatoria.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_BIND_ADDR", None),
            ("APP_LOG_FORMAT", None),
            ("APP_RUST_LOG", None),
        ],
        || {
            let config = Config::from_env().expect("config válida");
            assert_eq!(config.bind_addr.port(), 3000);
            assert_eq!(config.log_format, LogFormat::Pretty);
        },
    );

    // 3. Formato JSON aceptado.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_LOG_FORMAT", Some("json")),
        ],
        || {
            let config = Config::from_env().expect("config válida");
            assert_eq!(config.log_format, LogFormat::Json);
        },
    );

    // 4. Formato inválido: error, no fallback silencioso.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_LOG_FORMAT", Some("yaml")),
        ],
        || {
            let err = Config::from_env().expect_err("formato inválido debe fallar");
            assert!(err.to_string().contains("APP_LOG_FORMAT"));
        },
    );

    // 5. Bind addr inválido: error, no default silencioso.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_BIND_ADDR", Some("no-es-una-direccion")),
        ],
        || {
            let err = Config::from_env().expect_err("bind inválido debe fallar");
            assert!(err.to_string().contains("APP_BIND_ADDR"));
        },
    );
}
