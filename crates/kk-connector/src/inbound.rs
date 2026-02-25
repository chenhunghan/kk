use std::path::Path;

use tokio::sync::mpsc;
use tracing::{error, info};

use kk_core::nq;
use kk_core::types::InboundMessage;

use crate::config::ConnectorConfig;
use crate::groups::GroupMap;
use crate::provider::ConnectorEvent;

pub async fn process_inbound(
    mut rx: mpsc::Receiver<ConnectorEvent>,
    config: &ConnectorConfig,
    mut group_map: GroupMap,
) {
    let channel_type = config.channel_type_enum();
    let slug_prefix = config.slug_prefix();

    while let Some(event) = rx.recv().await {
        match event {
            ConnectorEvent::NewChat {
                chat_id,
                chat_title,
            } => {
                let slug = group_map.register(&chat_id, slug_prefix);
                if let Err(e) = group_map.persist(&config.groups_d_file, &config.channel_name) {
                    error!(error = %e, "failed to persist groups.d file");
                }
                info!(chat_id, slug, ?chat_title, "auto-registered new chat");
            }
            ConnectorEvent::Message(raw) => {
                let group = match group_map.resolve(&raw.chat_id) {
                    Some(g) => g,
                    None => {
                        let slug = group_map.register(&raw.chat_id, slug_prefix);
                        if let Err(e) =
                            group_map.persist(&config.groups_d_file, &config.channel_name)
                        {
                            error!(error = %e, "failed to persist groups.d file");
                        }
                        info!(
                            chat_id = %raw.chat_id, slug,
                            "auto-registered from @mention"
                        );
                        slug
                    }
                };

                let msg = InboundMessage {
                    channel: config.channel_name.clone(),
                    channel_type: channel_type.clone(),
                    group,
                    thread_id: raw.thread_id,
                    sender: raw.sender_name,
                    text: raw.text,
                    timestamp: raw.timestamp,
                    meta: raw.meta,
                };

                let payload = match serde_json::to_vec(&msg) {
                    Ok(p) => p,
                    Err(e) => {
                        error!(error = %e, "failed to serialize inbound message");
                        continue;
                    }
                };

                let inbound_dir = Path::new(&config.inbound_dir);
                match nq::enqueue(inbound_dir, msg.timestamp, &payload) {
                    Ok(path) => {
                        info!(
                            group = msg.group,
                            sender = msg.sender,
                            text_len = msg.text.len(),
                            file = %path.display(),
                            "enqueued inbound message"
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "failed to enqueue inbound message");
                    }
                }
            }
        }
    }
}
