//! Observabilidad: un subscriber global, formato legible en dev y JSON en prod.

use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

pub fn init(log_format: LogFormat, rust_log: &str) {
    let filter = EnvFilter::try_new(rust_log).unwrap_or_else(|_| EnvFilter::new("info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    // Se reintenta con `info` si ya había un subscriber instalado (ocurre en
    // tests). Un logger duplicado no debe tumbar el proceso.
    let result = match log_format {
        LogFormat::Json => builder.json().try_init(),
        LogFormat::Pretty => builder.try_init(),
    };

    if let Err(err) = result {
        eprintln!("advertencia: no se pudo instalar el subscriber de tracing: {err}");
    }
}
