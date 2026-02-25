//! Inbound loop: polls /data/inbound/ and routes messages.

use anyhow::Result;
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::{InboundMessage, TriggerMode};

use crate::state::SharedState;

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.inbound_poll_interval_ms);
    info!(
        interval_ms = state.config.inbound_poll_interval_ms,
        "inbound loop started"
    );

    loop {
        if let Err(e) = poll_once(&state).await {
            error!(error = %e, "inbound poll error");
        }
        sleep(interval).await;
    }
}

async fn poll_once(state: &SharedState) -> Result<()> {
    let inbound_dir = state.paths.inbound_dir();
    let files = nq::list_pending(&inbound_dir)?;

    if files.is_empty() {
        return Ok(());
    }

    for file_path in files {
        let raw = match nq::read_message(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "failed to read inbound message");
                continue;
            }
        };

        let msg: InboundMessage = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "invalid inbound JSON, discarding");
                let _ = nq::delete(&file_path);
                continue;
            }
        };

        // Check staleness
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now.saturating_sub(msg.timestamp) > state.config.stale_message_timeout {
            warn!(file = %file_path.display(), age = now - msg.timestamp, "stale inbound message, discarding");
            let _ = nq::delete(&file_path);
            continue;
        }

        // Route the message
        if let Err(e) = route_message(state, &msg).await {
            error!(group = msg.group, error = %e, "failed to route message");
        }

        // Delete processed file
        let _ = nq::delete(&file_path);
    }

    Ok(())
}

async fn route_message(state: &SharedState, msg: &InboundMessage) -> Result<()> {
    let groups = state.groups_config.read().await;
    let group_config = match groups.groups.get(&msg.group) {
        Some(config) => config,
        None => {
            debug!(
                group = msg.group,
                "message from unregistered group, skipping"
            );
            return Ok(());
        }
    };

    // Check trigger
    let should_trigger = match group_config.trigger_mode {
        TriggerMode::Always => true,
        TriggerMode::Mention => group_config
            .trigger_pattern
            .as_ref()
            .map(|p| msg.text.contains(p.as_str()))
            .unwrap_or(false),
        TriggerMode::Prefix => group_config
            .trigger_pattern
            .as_ref()
            .map(|p| msg.text.starts_with(p.as_str()))
            .unwrap_or(false),
    };

    if !should_trigger {
        debug!(
            group = msg.group,
            trigger_mode = ?group_config.trigger_mode,
            "message did not match trigger, skipping"
        );
        return Ok(());
    }

    // Check if a job is already running for this group
    let active_jobs = state.active_jobs.read().await;
    if active_jobs.contains_key(&msg.group) {
        drop(active_jobs);
        hot_path(state, msg).await
    } else {
        drop(active_jobs);
        cold_path(state, msg).await
    }
}

async fn cold_path(_state: &SharedState, msg: &InboundMessage) -> Result<()> {
    let session_id = kk_core::paths::session_id(&msg.group, msg.timestamp);
    let job_name = kk_core::paths::job_name(&session_id);

    info!(
        group = msg.group,
        session_id = session_id,
        job_name = job_name,
        "COLD PATH: creating Agent Job"
    );

    // TODO: implement cold path
    // 1. Create results directory + request.json
    // 2. Ensure per-group queue directory exists
    // 3. Strip trigger pattern from prompt
    // 4. Create K8s Job
    // 5. Update activeJobs

    Ok(())
}

async fn hot_path(state: &SharedState, msg: &InboundMessage) -> Result<()> {
    let active_jobs = state.active_jobs.read().await;
    let job_info = active_jobs.get(&msg.group).cloned();
    drop(active_jobs);

    if let Some(job_info) = job_info {
        info!(
            group = msg.group,
            job_name = job_info.job_name,
            "HOT PATH: forwarding to running Job"
        );

        // TODO: implement hot path
        // 1. Build follow-up message
        // 2. Atomic write to per-group queue
        // 3. Append to request.json messages array
    }

    Ok(())
}
