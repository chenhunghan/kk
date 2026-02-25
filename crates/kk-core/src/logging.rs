//! Structured JSON logging setup using tracing.

use tracing_subscriber::{EnvFilter, fmt};

/// Initialize structured JSON logging.
///
/// Log level is controlled by the `RUST_LOG` env var (defaults to `info`).
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .json()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .init();
}
