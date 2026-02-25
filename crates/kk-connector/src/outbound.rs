use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::OutboundMessage;

use crate::provider::ChatProvider;

pub async fn poll_outbound(
    outbox_dir: &str,
    channel_name: &str,
    sender: &dyn ChatProvider,
) -> Result<()> {
    let dir = Path::new(outbox_dir);

    cleanup_stale_stream_states(dir);

    let files = nq::list_pending(dir)?;

    for file_path in files {
        let raw = match nq::read_message(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "failed to read outbound message");
                continue;
            }
        };

        let msg: OutboundMessage = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                error!(file = %file_path.display(), error = %e, "malformed outbound message, discarding");
                let _ = nq::delete(&file_path);
                continue;
            }
        };

        if msg.channel != channel_name {
            error!(
                expected = channel_name,
                got = msg.channel,
                "outbound channel mismatch, discarding"
            );
            let _ = nq::delete(&file_path);
            continue;
        }

        let is_streaming = msg
            .meta
            .get("streaming")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let session_id = msg.meta.get("session_id").and_then(|v| v.as_str());

        let result = if is_streaming {
            send_streaming(dir, sender, &msg, session_id).await
        } else {
            send_final(dir, sender, &msg, session_id).await
        };

        match result {
            Ok(()) => {
                if is_streaming {
                    debug!(
                        group = msg.group,
                        thread_id = ?msg.thread_id,
                        text_len = msg.text.len(),
                        "sent streaming update"
                    );
                } else {
                    info!(
                        group = msg.group,
                        thread_id = ?msg.thread_id,
                        text_len = msg.text.len(),
                        "sent outbound message"
                    );
                }
                let _ = nq::delete(&file_path);
            }
            Err(e) => {
                error!(
                    file = %file_path.display(),
                    error = %e,
                    streaming = is_streaming,
                    "failed to send outbound message"
                );
                if let Ok(age) = nq::file_age_secs(&file_path)
                    && age > 300
                {
                    warn!(file = %file_path.display(), age_secs = age, "outbound message stuck for >5min");
                }
            }
        }
    }

    Ok(())
}

/// Handle a streaming intermediate message: send new or edit existing.
async fn send_streaming(
    outbox_dir: &Path,
    sender: &dyn ChatProvider,
    msg: &OutboundMessage,
    session_id: Option<&str>,
) -> Result<()> {
    let Some(sid) = session_id else {
        // No session_id — can't track stream state, send as regular message
        return sender.send(msg).await;
    };

    let state_path = stream_state_path(outbox_dir, sid);
    if state_path.exists() {
        // Edit existing streaming message
        let platform_msg_id = std::fs::read_to_string(&state_path)?;
        sender.edit(msg, platform_msg_id.trim()).await
    } else {
        // First streaming message: send new, capture platform message ID
        let msg_id = sender.send_returning_id(msg).await?;
        std::fs::write(&state_path, &msg_id)?;
        Ok(())
    }
}

/// Handle a final (non-streaming) message: edit if stream exists, otherwise send new.
async fn send_final(
    outbox_dir: &Path,
    sender: &dyn ChatProvider,
    msg: &OutboundMessage,
    session_id: Option<&str>,
) -> Result<()> {
    if let Some(sid) = session_id {
        let state_path = stream_state_path(outbox_dir, sid);
        if state_path.exists() {
            // Edit the streaming message with final content, then clean up
            let platform_msg_id = std::fs::read_to_string(&state_path)?;
            let result = sender.edit(msg, platform_msg_id.trim()).await;
            let _ = std::fs::remove_file(&state_path);
            return result;
        }
    }

    // No active stream — send as a new message
    sender.send(msg).await
}

/// Path for tracking the platform message ID of an active streaming session.
fn stream_state_path(outbox_dir: &Path, session_id: &str) -> PathBuf {
    outbox_dir.join(format!(".stream-{session_id}"))
}

/// Remove `.stream-*` files older than 10 minutes (crashed/stuck sessions).
fn cleanup_stale_stream_states(outbox_dir: &Path) {
    let entries = match std::fs::read_dir(outbox_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".stream-")
            && let Ok(age) = nq::file_age_secs(&entry.path())
            && age > 600
        {
            info!(file = %entry.path().display(), "cleaning stale stream state");
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
