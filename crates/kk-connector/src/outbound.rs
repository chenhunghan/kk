use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::OutboundMessage;

use crate::provider::ChatProvider;

/// Poll the outbox directory for non-streaming messages and send them.
pub async fn poll_outbound(
    outbox_dir: &str,
    channel_name: &str,
    sender: &dyn ChatProvider,
) -> Result<()> {
    let dir = Path::new(outbox_dir);
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

        match sender.send(&msg).await {
            Ok(()) => {
                info!(
                    group = msg.group,
                    thread_id = ?msg.thread_id,
                    text_len = msg.text.len(),
                    "sent outbound message"
                );
                let _ = nq::delete(&file_path);
            }
            Err(e) => {
                error!(
                    file = %file_path.display(),
                    error = %e,
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

/// Poll the stream directory for streaming updates and send/edit them.
///
/// Stream files are named by session_id and contain an `OutboundMessage` JSON
/// with `meta.final` indicating whether this is the last update.
/// Platform message ID state is tracked in `.stream-{session_id}` dotfiles.
pub async fn poll_stream(stream_dir: &str, sender: &dyn ChatProvider) -> Result<()> {
    let dir = Path::new(stream_dir);

    cleanup_stale_stream_states(dir);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip dotfiles (.stream-* state files, .tmp files)
        if name.starts_with('.') {
            continue;
        }

        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(e) => {
                warn!(session_id = name, error = %e, "failed to read stream file");
                continue;
            }
        };

        let msg: OutboundMessage = match serde_json::from_slice(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!(session_id = name, error = %e, "malformed stream file, removing");
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
        };

        let is_final = msg
            .meta
            .get("final")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let session_id = &name;
        let state_path = stream_state_path(dir, session_id);

        let result = if state_path.exists() {
            // We've already sent the initial message — edit it
            let platform_msg_id = std::fs::read_to_string(&state_path)?;
            let platform_msg_id = platform_msg_id.trim();
            let edit_result = sender.edit(&msg, platform_msg_id).await;
            if is_final {
                let _ = std::fs::remove_file(&state_path);
                let _ = std::fs::remove_file(entry.path());
            }
            edit_result
        } else if is_final {
            // Final without prior streaming — send as new message, then cleanup
            let result = sender.send(&msg).await;
            let _ = std::fs::remove_file(entry.path());
            result
        } else {
            // First streaming message — send new, capture platform message ID
            match sender.send_returning_id(&msg).await {
                Ok(msg_id) => {
                    std::fs::write(&state_path, &msg_id)?;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };

        match &result {
            Ok(()) => {
                if is_final {
                    info!(session_id, "stream finalized");
                } else {
                    debug!(session_id, text_len = msg.text.len(), "stream update sent");
                }
            }
            Err(e) => {
                error!(session_id, error = %e, "stream send/edit failed");
            }
        }
    }

    Ok(())
}

/// Path for tracking the platform message ID of an active streaming session.
fn stream_state_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!(".stream-{session_id}"))
}

/// Remove `.stream-*` files older than 10 minutes (crashed/stuck sessions).
fn cleanup_stale_stream_states(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Recorded call from MockChatProvider.
    #[derive(Debug, Clone)]
    enum MockCall {
        Send {
            text: String,
        },
        SendReturningId {
            text: String,
        },
        Edit {
            text: String,
            platform_msg_id: String,
        },
    }

    /// A ChatProvider that records all calls for assertion.
    struct MockChatProvider {
        calls: Arc<Mutex<Vec<MockCall>>>,
        /// The message ID returned by send_returning_id.
        mock_msg_id: String,
    }

    impl MockChatProvider {
        fn new(mock_msg_id: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                mock_msg_id: mock_msg_id.to_string(),
            }
        }

        fn calls(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatProvider for MockChatProvider {
        async fn send(&self, msg: &OutboundMessage) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Send {
                text: msg.text.clone(),
            });
            Ok(())
        }

        async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
            self.calls.lock().unwrap().push(MockCall::SendReturningId {
                text: msg.text.clone(),
            });
            Ok(self.mock_msg_id.clone())
        }

        async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Edit {
                text: msg.text.clone(),
                platform_msg_id: platform_msg_id.to_string(),
            });
            Ok(())
        }
    }

    fn write_stream_file(dir: &Path, session_id: &str, text: &str, is_final: bool) {
        let msg = OutboundMessage {
            channel: "test-channel".into(),
            group: "test-group".into(),
            thread_id: None,
            text: text.into(),
            meta: serde_json::json!({ "final": is_final, "chat_id": "-100" }),
        };
        std::fs::write(dir.join(session_id), serde_json::to_vec(&msg).unwrap()).unwrap();
    }

    /// First stream file → send_returning_id called, .stream-{sid} state file created.
    #[tokio::test]
    async fn stream_first_message_sends_new() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();

        write_stream_file(stream_dir, "sess-001", "thinking...", false);

        let mock = MockChatProvider::new("plat-msg-42");
        poll_stream(stream_dir.to_str().unwrap(), &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], MockCall::SendReturningId { text } if text == "thinking..."));

        // State file should be created with platform message ID
        let state = std::fs::read_to_string(stream_dir.join(".stream-sess-001")).unwrap();
        assert_eq!(state, "plat-msg-42");

        // Stream file should still exist (not final)
        assert!(stream_dir.join("sess-001").exists());
    }

    /// Subsequent stream file → edit called with saved platform message ID.
    #[tokio::test]
    async fn stream_subsequent_edits_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();

        // Simulate prior state: stream file + state file
        write_stream_file(stream_dir, "sess-002", "updated response...", false);
        std::fs::write(stream_dir.join(".stream-sess-002"), "plat-msg-99").unwrap();

        let mock = MockChatProvider::new("unused");
        poll_stream(stream_dir.to_str().unwrap(), &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            MockCall::Edit { text, platform_msg_id }
            if text == "updated response..." && platform_msg_id == "plat-msg-99"
        ));

        // Both files should still exist (not final)
        assert!(stream_dir.join("sess-002").exists());
        assert!(stream_dir.join(".stream-sess-002").exists());
    }

    /// Final stream file → edit called, then both stream file and state file deleted.
    #[tokio::test]
    async fn stream_final_edits_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();

        // Simulate active stream with state file
        write_stream_file(stream_dir, "sess-003", "The final answer is 42.", true);
        std::fs::write(stream_dir.join(".stream-sess-003"), "plat-msg-77").unwrap();

        let mock = MockChatProvider::new("unused");
        poll_stream(stream_dir.to_str().unwrap(), &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            MockCall::Edit { text, platform_msg_id }
            if text == "The final answer is 42." && platform_msg_id == "plat-msg-77"
        ));

        // Both files should be cleaned up
        assert!(
            !stream_dir.join("sess-003").exists(),
            "stream file should be deleted"
        );
        assert!(
            !stream_dir.join(".stream-sess-003").exists(),
            "state file should be deleted"
        );
    }

    /// Final without prior streaming → send (not edit), stream file deleted.
    #[tokio::test]
    async fn stream_final_without_prior_sends_new() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();

        // No state file — agent finished before any streaming happened
        write_stream_file(stream_dir, "sess-004", "instant response", true);

        let mock = MockChatProvider::new("unused");
        poll_stream(stream_dir.to_str().unwrap(), &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], MockCall::Send { text } if text == "instant response"));

        // Stream file should be cleaned up
        assert!(
            !stream_dir.join("sess-004").exists(),
            "stream file should be deleted"
        );
    }

    /// Full lifecycle: first send → edit → final edit + cleanup.
    #[tokio::test]
    async fn stream_full_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();
        let dir_str = stream_dir.to_str().unwrap();

        let mock = MockChatProvider::new("plat-msg-55");

        // Step 1: First streaming message
        write_stream_file(stream_dir, "sess-lifecycle", "thinking...", false);
        poll_stream(dir_str, &mock).await.unwrap();

        assert_eq!(mock.calls().len(), 1);
        assert!(
            matches!(&mock.calls()[0], MockCall::SendReturningId { text } if text == "thinking...")
        );
        assert!(stream_dir.join(".stream-sess-lifecycle").exists());

        // Step 2: Updated streaming message
        write_stream_file(
            stream_dir,
            "sess-lifecycle",
            "Here's what I found so far...",
            false,
        );
        poll_stream(dir_str, &mock).await.unwrap();

        assert_eq!(mock.calls().len(), 2);
        assert!(matches!(
            &mock.calls()[1],
            MockCall::Edit { text, platform_msg_id }
            if text == "Here's what I found so far..." && platform_msg_id == "plat-msg-55"
        ));

        // Step 3: Final message
        write_stream_file(
            stream_dir,
            "sess-lifecycle",
            "The complete answer is 42.",
            true,
        );
        poll_stream(dir_str, &mock).await.unwrap();

        assert_eq!(mock.calls().len(), 3);
        assert!(matches!(
            &mock.calls()[2],
            MockCall::Edit { text, platform_msg_id }
            if text == "The complete answer is 42." && platform_msg_id == "plat-msg-55"
        ));

        // Everything cleaned up
        assert!(
            !stream_dir.join("sess-lifecycle").exists(),
            "stream file should be deleted after final"
        );
        assert!(
            !stream_dir.join(".stream-sess-lifecycle").exists(),
            "state file should be deleted after final"
        );
    }

    /// Malformed stream file → removed, no calls made.
    #[tokio::test]
    async fn stream_malformed_file_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path();

        std::fs::write(stream_dir.join("sess-bad"), b"not valid json").unwrap();

        let mock = MockChatProvider::new("unused");
        poll_stream(stream_dir.to_str().unwrap(), &mock)
            .await
            .unwrap();

        assert!(mock.calls().is_empty());
        assert!(
            !stream_dir.join("sess-bad").exists(),
            "malformed stream file should be removed"
        );
    }
}
