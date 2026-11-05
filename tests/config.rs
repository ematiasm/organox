//! Configuration tests.
//!
//! Deliberately one test: environment variables are process-global state,
//! and tests that mutate them in parallel interfere with each other.

use organox::config::{Config, LogFormat};

/// Sets environment variables, runs `f`, and restores their previous values.
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
    // 1. Without DATABASE_URL: explicit error, no startup.
    with_env(&[("DATABASE_URL", None)], || {
        let err = Config::from_env().expect_err("must fail without DATABASE_URL");
        assert!(
            err.to_string().contains("DATABASE_URL"),
            "the error must name the missing variable, got: {err}"
        );
    });

    // 2. Defaults when only the required URL is set.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_BIND_ADDR", None),
            ("APP_LOG_FORMAT", None),
            ("APP_RUST_LOG", None),
        ],
        || {
            let config = Config::from_env().expect("valid config");
            assert_eq!(config.bind_addr.port(), 3000);
            assert_eq!(config.log_format, LogFormat::Pretty);
        },
    );

    // 3. JSON format is accepted.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_LOG_FORMAT", Some("json")),
        ],
        || {
            let config = Config::from_env().expect("valid config");
            assert_eq!(config.log_format, LogFormat::Json);
        },
    );

    // 4. Invalid format: error, no silent fallback.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_LOG_FORMAT", Some("yaml")),
        ],
        || {
            let err = Config::from_env().expect_err("invalid format must fail");
            assert!(err.to_string().contains("APP_LOG_FORMAT"));
        },
    );

    // 5. Invalid bind address: error, no silent default.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_BIND_ADDR", Some("not-an-address")),
        ],
        || {
            let err = Config::from_env().expect_err("invalid bind address must fail");
            assert!(err.to_string().contains("APP_BIND_ADDR"));
        },
    );
}
