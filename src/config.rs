//! Configuración tipada, cargada una sola vez al arrancar.
//!
//! Regla: si falta una variable o tiene un valor inválido, el proceso no
//! arranca y el mensaje dice exactamente qué corregir. Nunca defaults
//! silenciosos para secretos ni para la URL de la base.

use std::net::SocketAddr;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub log_format: LogFormat,
    pub rust_log: String,
    /// `Secure` en la cookie de sesión. Default `true`; en dev con HTTP local
    /// se baja con `APP_COOKIE_SECURE=false`.
    pub cookie_secure: bool,
    /// Intentos fallidos por cuenta antes de bloquearla.
    pub login_max_failures: i32,
    /// Minutos que dura el bloqueo por cuenta.
    pub login_block_minutes: i64,
    /// Logins por minuto permitidos por IP.
    pub login_ip_max_per_minute: u32,
}

impl Config {
    pub fn account_policy(&self) -> crate::ratelimit::AccountPolicy {
        crate::ratelimit::AccountPolicy {
            max_failures: self.login_max_failures,
            window: std::time::Duration::from_secs(15 * 60),
            block_duration: std::time::Duration::from_secs(
                (self.login_block_minutes.max(1) * 60) as u64,
            ),
        }
    }

    pub fn ip_policy(&self) -> crate::ratelimit::IpPolicy {
        crate::ratelimit::IpPolicy {
            max_requests: self.login_ip_max_per_minute.max(1),
            window: std::time::Duration::from_secs(60),
            burst: 10,
        }
    }
}

impl Config {
    /// Lee la configuración del entorno. Pensado para llamarse una vez al inicio.
    pub fn from_env() -> Result<Self, AppError> {
        let database_url = required("DATABASE_URL")?;
        let bind_addr = parse("APP_BIND_ADDR", "0.0.0.0:3000")?;
        let log_format = match optional("APP_LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => {
                return Err(AppError::Config(format!(
                    "APP_LOG_FORMAT debe ser \"pretty\" o \"json\", recibido: {other:?}"
                )));
            }
        };
        let rust_log = optional("APP_RUST_LOG")
            .unwrap_or_else(|| "organox=debug,tower_http=debug,sqlx=warn".to_string());
        let cookie_secure = parse_bool("APP_COOKIE_SECURE", true)?;
        let login_max_failures = parse("APP_LOGIN_MAX_FAILURES", "5")?;
        let login_block_minutes = parse("APP_LOGIN_BLOCK_MINUTES", "5")?;
        let login_ip_max_per_minute = parse("APP_LOGIN_IP_MAX_PER_MINUTE", "30")?;

        if login_max_failures < 1 {
            return Err(AppError::Config(
                "APP_LOGIN_MAX_FAILURES debe ser al menos 1".to_string(),
            ));
        }

        Ok(Self {
            database_url,
            bind_addr,
            log_format,
            rust_log,
            cookie_secure,
            login_max_failures,
            login_block_minutes,
            login_ip_max_per_minute,
        })
    }
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required(key: &str) -> Result<String, AppError> {
    optional(key).ok_or_else(|| {
        AppError::Config(format!(
            "falta la variable de entorno {key}. Copiá .env.example a .env"
        ))
    })
}

fn parse_bool(key: &str, default: bool) -> Result<bool, AppError> {
    let Some(raw) = optional(key) else {
        return Ok(default);
    };
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => Err(AppError::Config(format!(
            "{key} debe ser true o false, recibido: {other:?}"
        ))),
    }
}

fn parse<T>(key: &str, default: &str) -> Result<T, AppError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = optional(key).unwrap_or_else(|| default.to_string());
    value
        .parse()
        .map_err(|e| AppError::Config(format!("{key} tiene un valor inválido ({value:?}): {e}")))
}
