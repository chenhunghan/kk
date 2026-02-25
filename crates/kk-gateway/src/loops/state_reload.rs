//! State reload loop: periodically reloads groups config from disk.

use anyhow::Result;
use tokio::time::{Duration, sleep};
use tracing::{error, info};

use crate::state::SharedState;

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.state_reload_interval_ms);
    info!(
        interval_ms = state.config.state_reload_interval_ms,
        "state reload loop started"
    );

    loop {
        sleep(interval).await;
        if let Err(e) = state.reload_groups_config().await {
            error!(error = %e, "failed to reload groups config");
        }
    }
}
