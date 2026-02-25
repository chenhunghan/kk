//! Cleanup loop: deletes finished K8s Jobs, handles stuck results, purges archives.

use anyhow::Result;
use tokio::time::{Duration, sleep};
use tracing::{error, info};

use crate::state::SharedState;

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.cleanup_interval_ms);
    info!(
        interval_ms = state.config.cleanup_interval_ms,
        "cleanup loop started"
    );

    loop {
        if let Err(e) = cleanup_once(&state).await {
            error!(error = %e, "cleanup error");
        }
        sleep(interval).await;
    }
}

async fn cleanup_once(_state: &SharedState) -> Result<()> {
    // TODO: implement cleanup tasks
    // 1. Clean up completed K8s Jobs past TTL
    // 2. Detect crashed Jobs (in activeJobs but K8s Job gone)
    // 3. Handle orphaned group queue files
    // 4. Purge archived results older than RESULTS_ARCHIVE_TTL

    Ok(())
}
