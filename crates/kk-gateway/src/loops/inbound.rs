//! Inbound loop: polls /data/inbound/ and routes messages.

use anyhow::{Context, Result};
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::{
    FollowUpMessage, InboundMessage, RequestManifest, RequestMessage, TriggerMode,
};

use crate::state::{self, ActiveJob, SharedState};

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

pub async fn poll_once(state: &SharedState) -> Result<()> {
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

    // Check for stop command
    if is_stop_command(&msg.text) {
        return handle_stop(state, msg).await;
    }

    // Build thread-aware routing key
    let key = state::routing_key(&msg.group, msg.thread_id.as_deref());

    let active_jobs = state.active_jobs.read().await;
    if active_jobs.contains_key(&key) {
        drop(active_jobs);
        hot_path(state, msg, &key).await
    } else {
        drop(active_jobs);
        // Strip trigger pattern from prompt text
        let prompt = strip_trigger(
            &msg.text,
            &group_config.trigger_mode,
            group_config.trigger_pattern.as_deref(),
        );
        cold_path(state, msg, &key, &prompt).await
    }
}

/// Check if a message is a stop command.
fn is_stop_command(text: &str) -> bool {
    let trimmed = text.trim().to_lowercase();
    trimmed == "/stop" || trimmed == "stop"
}

/// Handle a stop command: write sentinel file to the group queue directory.
async fn handle_stop(state: &SharedState, msg: &InboundMessage) -> Result<()> {
    let key = state::routing_key(&msg.group, msg.thread_id.as_deref());
    let active_jobs = state.active_jobs.read().await;

    if !active_jobs.contains_key(&key) {
        debug!(
            group = msg.group,
            "stop command but no active Job, ignoring"
        );
        return Ok(());
    }
    drop(active_jobs);

    info!(group = msg.group, "STOP: writing stop sentinel");
    let stop_path = state
        .paths
        .stop_sentinel(&msg.group, msg.thread_id.as_deref());
    if let Some(parent) = stop_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&stop_path, "stop").context("write stop sentinel")?;

    Ok(())
}

/// Strip trigger pattern from message text.
fn strip_trigger(text: &str, trigger_mode: &TriggerMode, trigger_pattern: Option<&str>) -> String {
    match (trigger_mode, trigger_pattern) {
        (TriggerMode::Prefix, Some(pattern)) => text
            .strip_prefix(pattern)
            .unwrap_or(text)
            .trim()
            .to_string(),
        (TriggerMode::Mention, Some(pattern)) => text.replace(pattern, "").trim().to_string(),
        _ => text.to_string(),
    }
}

/// Cold path: no active Job for this routing key — launch a new Agent.
async fn cold_path(
    state: &SharedState,
    msg: &InboundMessage,
    routing_key: &str,
    prompt: &str,
) -> Result<()> {
    let session_id =
        kk_core::paths::session_id_threaded(&msg.group, msg.thread_id.as_deref(), msg.timestamp);
    let job_name = kk_core::paths::job_name(&session_id);

    info!(
        group = msg.group,
        session_id, job_name, "COLD PATH: launching Agent"
    );

    // 1. Create results directory + request.json
    let results_dir = state.paths.results_dir(&session_id);
    std::fs::create_dir_all(&results_dir)
        .with_context(|| format!("failed to create results dir: {}", results_dir.display()))?;

    let manifest = RequestManifest {
        channel: msg.channel.clone(),
        group: msg.group.clone(),
        thread_id: msg.thread_id.clone(),
        sender: msg.sender.clone(),
        meta: msg.meta.clone(),
        messages: vec![RequestMessage {
            sender: msg.sender.clone(),
            text: prompt.to_string(),
            ts: msg.timestamp,
        }],
    };
    let manifest_path = state.paths.request_manifest(&session_id);
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("failed to write request.json: {}", manifest_path.display()))?;

    // Write initial status
    let status_path = state.paths.result_status(&session_id);
    std::fs::write(&status_path, "running")?;

    // 2. Ensure per-group queue directory exists (thread-aware)
    let queue_dir = state
        .paths
        .group_queue_dir_threaded(&msg.group, msg.thread_id.as_deref());
    std::fs::create_dir_all(&queue_dir)?;

    // 3. Launch agent via the launcher abstraction
    let agent = state
        .launcher
        .launch(&job_name, &session_id, msg, &state.config)
        .await
        .with_context(|| format!("failed to launch agent {job_name}"))?;

    // 4. Update activeJobs
    let mut active = state.active_jobs.write().await;
    active.insert(
        routing_key.to_string(),
        ActiveJob {
            job_name: agent.handle,
            session_id,
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: agent.created_at,
        },
    );

    info!(job = job_name, routing_key, "Agent launched");
    Ok(())
}

/// Hot path: an Agent Job is already running — forward as follow-up.
async fn hot_path(state: &SharedState, msg: &InboundMessage, routing_key: &str) -> Result<()> {
    let active_jobs = state.active_jobs.read().await;
    let job_info = active_jobs.get(routing_key).cloned();
    drop(active_jobs);

    let job_info = match job_info {
        Some(j) => j,
        None => return Ok(()),
    };

    info!(
        group = msg.group,
        job_name = job_info.job_name,
        "HOT PATH: forwarding to running Job"
    );

    // 1. Build follow-up message
    let follow_up = FollowUpMessage {
        sender: msg.sender.clone(),
        text: msg.text.clone(),
        timestamp: msg.timestamp,
        channel: msg.channel.clone(),
        thread_id: msg.thread_id.clone(),
        meta: msg.meta.clone(),
    };

    // 2. Atomic write to per-group queue
    let queue_dir = state
        .paths
        .group_queue_dir_threaded(&msg.group, msg.thread_id.as_deref());
    let payload = serde_json::to_vec(&follow_up)?;
    nq::enqueue(&queue_dir, msg.timestamp, &payload)?;

    // 3. Append to request.json messages array (bookkeeping)
    let manifest_path = state.paths.request_manifest(&job_info.session_id);
    if manifest_path.exists()
        && let Ok(content) = std::fs::read_to_string(&manifest_path)
        && let Ok(mut manifest) = serde_json::from_str::<RequestManifest>(&content)
    {
        manifest.messages.push(RequestMessage {
            sender: msg.sender.clone(),
            text: msg.text.clone(),
            ts: msg.timestamp,
        });
        let _ = std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_trigger_prefix() {
        let result = strip_trigger("/ask what is rust?", &TriggerMode::Prefix, Some("/ask"));
        assert_eq!(result, "what is rust?");
    }

    #[test]
    fn test_strip_trigger_mention() {
        let result = strip_trigger(
            "hey @bot what is rust?",
            &TriggerMode::Mention,
            Some("@bot"),
        );
        assert_eq!(result, "hey  what is rust?");
    }

    #[test]
    fn test_strip_trigger_always() {
        let result = strip_trigger("what is rust?", &TriggerMode::Always, None);
        assert_eq!(result, "what is rust?");
    }

    #[test]
    fn test_is_stop_command() {
        assert!(is_stop_command("/stop"));
        assert!(is_stop_command("stop"));
        assert!(is_stop_command("STOP"));
        assert!(is_stop_command("/Stop"));
        assert!(is_stop_command("  /stop  "));
        assert!(!is_stop_command("please stop"));
        assert!(!is_stop_command("/stop now"));
        assert!(!is_stop_command("don't stop"));
    }
}
