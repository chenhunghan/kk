//! Results loop: polls /data/results/*/status for completed Agent Jobs.

use anyhow::Result;
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::types::ResultStatus;

use crate::state::SharedState;

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.results_poll_interval_ms);
    info!(
        interval_ms = state.config.results_poll_interval_ms,
        "results loop started"
    );

    loop {
        if let Err(e) = poll_once(&state).await {
            error!(error = %e, "results poll error");
        }
        sleep(interval).await;
    }
}

async fn poll_once(state: &SharedState) -> Result<()> {
    let results_base = state.paths.results_dir("");
    let parent = results_base.parent().unwrap();

    if !parent.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(parent)?;
    for entry in entries {
        let entry = entry?;
        let dir_name = entry.file_name().to_string_lossy().to_string();

        // Skip .done directory and non-directories
        if dir_name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }

        let status_path = state.paths.result_status(&dir_name);
        if !status_path.exists() {
            continue;
        }

        let status_str = std::fs::read_to_string(&status_path)?.trim().to_string();
        let status = match ResultStatus::parse(&status_str) {
            Some(s) => s,
            None => {
                warn!(dir = dir_name, status = status_str, "unknown result status");
                continue;
            }
        };

        match status {
            ResultStatus::Running => {
                debug!(dir = dir_name, "agent still running");
                continue;
            }
            ResultStatus::Done => {
                info!(dir = dir_name, "processing completed result");
                // TODO: implement result processing
                // 1. Read request.json for routing info
                // 2. Extract response text from response.jsonl
                // 3. Build OutboundMessage
                // 4. Write to outbox
                // 5. Archive results dir
                // 6. Remove from activeJobs
            }
            ResultStatus::Error => {
                warn!(dir = dir_name, "processing error result");
                // TODO: implement error handling
                // 1. Read request.json
                // 2. Build error outbound message
                // 3. Write to outbox
                // 4. Archive
                // 5. Remove from activeJobs
            }
        }
    }

    Ok(())
}
