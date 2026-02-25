//! Results loop: polls /data/results/*/status for completed Agent Jobs.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::{ContentBlock, OutboundMessage, RequestManifest, ResultLine, ResultStatus};

use crate::state::{self, SharedState};

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

pub async fn poll_once(state: &SharedState) -> Result<()> {
    let results_dir = state.paths.results_dir("");

    if !results_dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&results_dir)?;
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
                if let Err(e) = try_stream_partial(state, &dir_name).await {
                    debug!(dir = dir_name, error = %e, "stream partial error");
                }
                continue;
            }
            ResultStatus::Done => {
                info!(dir = dir_name, "processing completed result");
                if let Err(e) = process_done(state, &dir_name).await {
                    error!(dir = dir_name, error = %e, "failed to process done result");
                }
            }
            ResultStatus::Error => {
                warn!(dir = dir_name, "processing error result");
                if let Err(e) = process_error(state, &dir_name).await {
                    error!(dir = dir_name, error = %e, "failed to process error result");
                }
            }
            ResultStatus::Stopped => {
                info!(dir = dir_name, "processing stopped result");
                if let Err(e) = process_stopped(state, &dir_name).await {
                    error!(dir = dir_name, error = %e, "failed to process stopped result");
                }
            }
            ResultStatus::Overflow => {
                warn!(dir = dir_name, "processing overflow result");
                if let Err(e) = process_overflow(state, &dir_name).await {
                    error!(dir = dir_name, error = %e, "failed to process overflow result");
                }
            }
        }
    }

    Ok(())
}

/// Process a completed (done) Agent Job result.
async fn process_done(state: &SharedState, session_id: &str) -> Result<()> {
    // 1. Read request.json for routing info
    let manifest = read_manifest(state, session_id)?;

    // 2. Extract response text from response.jsonl
    let response_path = state.paths.result_response(session_id);
    let text = extract_response_text(&response_path)?;

    // 3. Build and write OutboundMessage
    write_outbound(state, &manifest, &text)?;

    // 4. Archive results dir
    archive_results(state, session_id)?;

    // 5. Remove from activeJobs + clean up stream offset
    remove_from_active(state, &manifest).await;
    state.stream_offsets.write().await.remove(session_id);

    info!(session_id, channel = manifest.channel, "result delivered");
    Ok(())
}

/// Process an error Agent Job result.
async fn process_error(state: &SharedState, session_id: &str) -> Result<()> {
    // 1. Read request.json for routing info
    let manifest = read_manifest(state, session_id)?;

    // 2. Build error outbound message
    let text = format!("[kk] Agent job failed for session {session_id}. Check logs for details.");
    write_outbound(state, &manifest, &text)?;

    // 3. Archive results dir
    archive_results(state, session_id)?;

    // 4. Remove from activeJobs + clean up stream offset
    remove_from_active(state, &manifest).await;
    state.stream_offsets.write().await.remove(session_id);

    warn!(
        session_id,
        channel = manifest.channel,
        "error result delivered"
    );
    Ok(())
}

/// Process a user-stopped Agent Job result.
async fn process_stopped(state: &SharedState, session_id: &str) -> Result<()> {
    let manifest = read_manifest(state, session_id)?;

    let response_path = state.paths.result_response(session_id);
    let partial_text = extract_response_text(&response_path)?;

    let text = if partial_text == "(no response)" || partial_text == "(no response file)" {
        "[kk] Agent stopped by user.".to_string()
    } else {
        format!("{partial_text}\n\n[kk] (stopped by user)")
    };

    write_outbound(state, &manifest, &text)?;
    archive_results(state, session_id)?;
    remove_from_active(state, &manifest).await;
    state.stream_offsets.write().await.remove(session_id);

    info!(
        session_id,
        channel = manifest.channel,
        "stopped result delivered"
    );
    Ok(())
}

/// Process a context overflow result.
async fn process_overflow(state: &SharedState, session_id: &str) -> Result<()> {
    let manifest = read_manifest(state, session_id)?;

    let response_path = state.paths.result_response(session_id);
    let partial_text = extract_response_text(&response_path)?;

    let text = if partial_text == "(no response)" || partial_text == "(no response file)" {
        "[kk] Context window overflow. The conversation has grown too long. Please start a new conversation.".to_string()
    } else {
        format!(
            "{partial_text}\n\n[kk] Context window overflow. Please start a new conversation for further questions."
        )
    };

    write_outbound(state, &manifest, &text)?;

    // Clear persisted claude session_id so next Job starts fresh
    let sid_path = state
        .paths
        .claude_session_id_file(&manifest.group, manifest.thread_id.as_deref());
    let _ = std::fs::remove_file(&sid_path);

    archive_results(state, session_id)?;
    remove_from_active(state, &manifest).await;
    state.stream_offsets.write().await.remove(session_id);

    warn!(
        session_id,
        channel = manifest.channel,
        "overflow result delivered, session reset"
    );
    Ok(())
}

/// Read new lines from response.jsonl since last offset, send intermediate messages.
async fn try_stream_partial(state: &SharedState, session_id: &str) -> Result<()> {
    let response_path = state.paths.result_response(session_id);
    if !response_path.exists() {
        return Ok(());
    }

    let metadata = std::fs::metadata(&response_path)?;
    let file_len = metadata.len();

    let offsets = state.stream_offsets.read().await;
    let last_offset = offsets.get(session_id).copied().unwrap_or(0);
    drop(offsets);

    if file_len <= last_offset {
        return Ok(());
    }

    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(&response_path)?;
    file.seek(SeekFrom::Start(last_offset))?;
    let mut new_bytes = Vec::new();
    file.read_to_end(&mut new_bytes)?;

    let new_text = String::from_utf8_lossy(&new_bytes);

    // Extract the latest assistant/result text from new lines
    let mut latest_text: Option<String> = None;
    for line in new_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rl) = serde_json::from_str::<ResultLine>(line) {
            if rl.line_type == "result" {
                if let Some(text) = rl.result {
                    latest_text = Some(text);
                }
            } else if rl.line_type == "assistant"
                && let Some(msg) = rl.message
            {
                for block in msg.content {
                    if let ContentBlock::Text { text } = block {
                        latest_text = Some(text);
                    }
                }
            }
        }
    }

    // Update offset
    state
        .stream_offsets
        .write()
        .await
        .insert(session_id.to_string(), file_len);

    // Send intermediate message if we found new text
    if let Some(text) = latest_text {
        let manifest = match read_manifest(state, session_id) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };

        // Flatten manifest meta into top level so providers can read chat_id/channel_id directly,
        // then add streaming + session_id fields.
        let meta = {
            let mut m = manifest.meta.clone();
            if let Some(obj) = m.as_object_mut() {
                obj.insert("streaming".into(), serde_json::Value::Bool(true));
                obj.insert(
                    "session_id".into(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            m
        };

        let outbound = OutboundMessage {
            channel: manifest.channel.clone(),
            group: manifest.group.clone(),
            thread_id: manifest.thread_id.clone(),
            text,
            meta,
        };

        let outbox_dir = state.paths.outbox_dir(&manifest.channel);
        let payload = serde_json::to_vec(&outbound)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        nq::enqueue(&outbox_dir, now, &payload)?;

        debug!(session_id, "sent streaming partial response");
    }

    Ok(())
}

fn read_manifest(state: &SharedState, session_id: &str) -> Result<RequestManifest> {
    let path = state.paths.request_manifest(session_id);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read request.json: {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse request.json")
}

/// Extract the final response text from response.jsonl.
/// Strategy: last "result" line wins; fallback to last "assistant" text block.
fn extract_response_text(jsonl_path: &Path) -> Result<String> {
    if !jsonl_path.exists() {
        return Ok("(no response file)".to_string());
    }

    let content = std::fs::read_to_string(jsonl_path)?;
    let mut last_result: Option<String> = None;
    let mut last_assistant_text: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rl) = serde_json::from_str::<ResultLine>(line) {
            if rl.line_type == "result" {
                if let Some(text) = rl.result {
                    last_result = Some(text);
                }
            } else if rl.line_type == "assistant"
                && let Some(msg) = rl.message
            {
                for block in msg.content {
                    if let ContentBlock::Text { text } = block {
                        last_assistant_text = Some(text);
                    }
                }
            }
        }
    }

    Ok(last_result
        .or(last_assistant_text)
        .unwrap_or_else(|| "(no response)".to_string()))
}

fn write_outbound(state: &SharedState, manifest: &RequestManifest, text: &str) -> Result<()> {
    let outbound = OutboundMessage {
        channel: manifest.channel.clone(),
        group: manifest.group.clone(),
        thread_id: manifest.thread_id.clone(),
        text: text.to_string(),
        meta: manifest.meta.clone(),
    };

    let outbox_dir = state.paths.outbox_dir(&manifest.channel);
    let payload = serde_json::to_vec(&outbound)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    nq::enqueue(&outbox_dir, now, &payload)?;
    Ok(())
}

fn archive_results(state: &SharedState, session_id: &str) -> Result<()> {
    let src = state.paths.results_dir(session_id);
    let done_dir = state.paths.results_done_dir();
    std::fs::create_dir_all(&done_dir)?;
    let dst = done_dir.join(session_id);
    std::fs::rename(&src, &dst)
        .with_context(|| format!("failed to archive {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

async fn remove_from_active(state: &SharedState, manifest: &RequestManifest) {
    let key = state::routing_key(&manifest.group, manifest.thread_id.as_deref());
    let mut active = state.active_jobs.write().await;
    active.remove(&key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_extract_response_text_result_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"thinking..."}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"result","result":"The answer is 42."}}"#).unwrap();

        let text = extract_response_text(&path).unwrap();
        assert_eq!(text, "The answer is 42.");
    }

    #[test]
    fn test_extract_response_text_fallback_to_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"Here is my answer."}}]}}}}"#).unwrap();

        let text = extract_response_text(&path).unwrap();
        assert_eq!(text, "Here is my answer.");
    }

    #[test]
    fn test_extract_response_text_no_file() {
        let path = Path::new("/nonexistent/response.jsonl");
        let text = extract_response_text(path).unwrap();
        assert_eq!(text, "(no response file)");
    }

    #[test]
    fn test_extract_response_text_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        std::fs::write(&path, "").unwrap();

        let text = extract_response_text(&path).unwrap();
        assert_eq!(text, "(no response)");
    }
}
