use std::path::Path;

use anyhow::Result;
use tracing::{error, info, warn};

use kk_core::nq;
use kk_core::types::OutboundMessage;

use crate::provider::ProviderSender;

pub async fn poll_outbound(
    outbox_dir: &str,
    channel_name: &str,
    sender: &ProviderSender,
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
