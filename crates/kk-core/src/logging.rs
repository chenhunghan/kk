//! Structured logging setup using tracing.

use tracing_subscriber::{EnvFilter, fmt};
use std::env;

/// Initialize logging.
///
/// Log level is controlled by the `RUST_LOG` env var (defaults to `info`).
/// Log format is controlled by `LOG_FORMAT` (defaults to `pretty`, can be `json`).
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".into());

    let subscriber = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false);

    if format == "json" {
        subscriber.json().init();
    } else {
        subscriber.init();
    }
}
