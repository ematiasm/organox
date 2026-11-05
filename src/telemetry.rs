//! Observability: one global subscriber, readable format in dev and JSON in prod.

use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

pub fn init(log_format: LogFormat, rust_log: &str) {
    let filter = EnvFilter::try_new(rust_log).unwrap_or_else(|_| EnvFilter::new("info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    // Retry with `info` if a subscriber was already installed (as happens in
    // tests). A duplicate logger must not crash the process.
    let result = match log_format {
        LogFormat::Json => builder.json().try_init(),
        LogFormat::Pretty => builder.try_init(),
    };

    if let Err(err) = result {
        eprintln!("warning: could not install the tracing subscriber: {err}");
    }
}
