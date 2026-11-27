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
            ("APP_DB_MAX_CONNECTIONS", None),
            ("APP_BIND_ADDR", None),
            ("APP_FRONTEND_DIR", None),
            ("APP_LOG_FORMAT", None),
            ("APP_RUST_LOG", None),
            ("APP_COOKIE_SECURE", None),
            ("APP_ENABLE_DOCS", None),
            ("APP_LOGIN_MAX_FAILURES", None),
            ("APP_LOGIN_BLOCK_MINUTES", None),
            ("APP_LOGIN_IP_MAX_PER_MINUTE", None),
        ],
        || {
            let config = Config::from_env().expect("valid config");
            assert_eq!(config.db_max_connections, 5);
            assert_eq!(config.bind_addr.port(), 3000);
            assert_eq!(config.frontend_dir, std::path::Path::new("frontend/build"));
            assert_eq!(config.log_format, LogFormat::Pretty);
            assert!(config.cookie_secure);
            assert!(!config.enable_docs);
            assert_eq!(config.login_max_failures, 5);
            assert_eq!(config.login_block_minutes, 5);
            assert_eq!(config.login_ip_max_per_minute, 30);
            assert_eq!(
                config.account_policy().block_duration,
                std::time::Duration::from_secs(300)
            );
            assert_eq!(config.ip_policy().max_requests, 30);
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

    // 6. APP_ENABLE_DOCS accepts booleans and rejects invalid values.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_ENABLE_DOCS", Some("true")),
        ],
        || {
            let config = Config::from_env().expect("valid config");
            assert!(config.enable_docs);
        },
    );
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_ENABLE_DOCS", Some("false")),
        ],
        || {
            let config = Config::from_env().expect("valid config");
            assert!(!config.enable_docs);
        },
    );
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_ENABLE_DOCS", Some("maybe")),
        ],
        || {
            let err = Config::from_env().expect_err("invalid APP_ENABLE_DOCS must fail");
            assert!(err.to_string().contains("APP_ENABLE_DOCS"));
        },
    );

    // 7. Rate-limit knobs fail fast when < 1 (no silent clamping).
    for (key, bad) in [
        ("APP_LOGIN_MAX_FAILURES", "0"),
        ("APP_LOGIN_MAX_FAILURES", "-3"),
        ("APP_LOGIN_BLOCK_MINUTES", "0"),
        ("APP_LOGIN_BLOCK_MINUTES", "-5"),
        ("APP_LOGIN_BLOCK_MINUTES", "9223372036854775807"),
        ("APP_LOGIN_IP_MAX_PER_MINUTE", "0"),
        ("APP_DB_MAX_CONNECTIONS", "0"),
    ] {
        with_env(
            &[
                ("DATABASE_URL", Some("postgres://localhost/x")),
                ("APP_DB_MAX_CONNECTIONS", None),
                ("APP_LOGIN_MAX_FAILURES", None),
                ("APP_LOGIN_BLOCK_MINUTES", None),
                ("APP_LOGIN_IP_MAX_PER_MINUTE", None),
                (key, Some(bad)),
            ],
            || {
                let err = Config::from_env()
                    .expect_err(&format!("{key}={bad} must fail fast instead of clamping"));
                assert!(
                    err.to_string().contains(key),
                    "error must name {key}, got: {err}"
                );
            },
        );
    }

    // 8. Configurable DB pool size and frontend directory.
    with_env(
        &[
            ("DATABASE_URL", Some("postgres://localhost/x")),
            ("APP_DB_MAX_CONNECTIONS", Some("12")),
            ("APP_FRONTEND_DIR", Some("/srv/organox/web")),
            ("APP_LOGIN_BLOCK_MINUTES", Some("10")),
            ("APP_LOGIN_IP_MAX_PER_MINUTE", Some("60")),
        ],
        || {
            let config = Config::from_env().expect("valid custom config");
            assert_eq!(config.db_max_connections, 12);
            assert_eq!(
                config.frontend_dir,
                std::path::Path::new("/srv/organox/web")
            );
            assert_eq!(
                config.account_policy().block_duration,
                std::time::Duration::from_secs(600)
            );
            assert_eq!(config.ip_policy().max_requests, 60);
        },
    );
}
