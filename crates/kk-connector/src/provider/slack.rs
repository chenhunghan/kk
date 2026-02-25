use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const SLACK_MAX_MESSAGE_LEN: usize = 4000;

/// Handle for sending outbound messages via Slack Web API.
#[derive(Clone)]
pub struct SlackOutbound {
    bot_token: String,
    client: reqwest::Client,
}

impl SlackOutbound {
    pub fn new(bot_token: String, client: reqwest::Client) -> Self {
        Self { bot_token, client }
    }
}

#[async_trait]
impl ChatProvider for SlackOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let chunks = split_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut body = serde_json::json!({
                "channel": channel_id,
                "text": chunk,
            });

            // Thread the reply if thread_id is set
            if let Some(ref thread_ts) = msg.thread_id {
                body["thread_ts"] = serde_json::Value::String(thread_ts.clone());
            }

            // If first chunk has a reply_to_ts and we're not already in a thread,
            // reply in a thread
            if i == 0
                && msg.thread_id.is_none()
                && let Some(reply_ts) = msg.meta.get("reply_to_ts").and_then(|v| v.as_str())
            {
                body["thread_ts"] = serde_json::Value::String(reply_ts.to_string());
            }

            let resp: serde_json::Value = self
                .client
                .post("https://slack.com/api/chat.postMessage")
                .bearer_auth(&self.bot_token)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("failed to send slack message to channel {channel_id}"))?
                .json()
                .await
                .context("failed to parse chat.postMessage response")?;

            if !resp["ok"].as_bool().unwrap_or(false) {
                let error = resp["error"].as_str().unwrap_or("unknown");
                bail!("chat.postMessage failed for channel {channel_id}: {error}");
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        let mut body = serde_json::json!({
            "channel": channel_id,
            "text": text,
        });

        if let Some(ref thread_ts) = msg.thread_id {
            body["thread_ts"] = serde_json::Value::String(thread_ts.clone());
        } else if let Some(reply_ts) = msg.meta.get("reply_to_ts").and_then(|v| v.as_str()) {
            body["thread_ts"] = serde_json::Value::String(reply_ts.to_string());
        }

        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to send slack message to channel {channel_id}"))?
            .json()
            .await
            .context("failed to parse chat.postMessage response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            bail!("chat.postMessage failed for channel {channel_id}: {error}");
        }

        let ts = resp["ts"]
            .as_str()
            .context("missing ts in chat.postMessage response")?;
        Ok(ts.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": platform_msg_id,
            "text": text,
        });

        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/chat.update")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to edit slack message {platform_msg_id} in channel {channel_id}")
            })?
            .json()
            .await
            .context("failed to parse chat.update response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            bail!("chat.update failed for channel {channel_id}: {error}");
        }

        Ok(())
    }

    fn supports_native_stream(&self) -> bool {
        true
    }

    async fn stream_start(&self, msg: &OutboundMessage) -> Result<String> {
        let channel_id = extract_channel_id(msg)?;
        let user_id = msg
            .meta
            .get("user_id")
            .and_then(|v| v.as_str())
            .context("outbound message missing meta.user_id for stream_start")?;
        let team_id = msg
            .meta
            .get("team_id")
            .and_then(|v| v.as_str())
            .context("outbound message missing meta.team_id for stream_start")?;

        let text = truncate_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        let mut body = serde_json::json!({
            "channel": channel_id,
            "text": text,
            "recipient_user_id": user_id,
            "recipient_team_id": team_id,
        });

        if let Some(ref thread_ts) = msg.thread_id {
            body["thread_ts"] = serde_json::Value::String(thread_ts.clone());
        } else if let Some(reply_ts) = msg.meta.get("reply_to_ts").and_then(|v| v.as_str()) {
            body["thread_ts"] = serde_json::Value::String(reply_ts.to_string());
        }

        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/chat.startStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to start slack stream in channel {channel_id}"))?
            .json()
            .await
            .context("failed to parse chat.startStream response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            bail!("chat.startStream failed for channel {channel_id}: {error}");
        }

        let ts = resp["ts"]
            .as_str()
            .context("missing ts in chat.startStream response")?;
        Ok(ts.to_string())
    }

    async fn stream_append(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": platform_msg_id,
            "text": text,
        });

        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/chat.appendStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to append slack stream {platform_msg_id} in channel {channel_id}")
            })?
            .json()
            .await
            .context("failed to parse chat.appendStream response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            bail!("chat.appendStream failed for channel {channel_id}: {error}");
        }

        Ok(())
    }

    async fn stream_stop(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, SLACK_MAX_MESSAGE_LEN);
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": platform_msg_id,
            "text": text,
        });

        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/chat.stopStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to stop slack stream {platform_msg_id} in channel {channel_id}")
            })?
            .json()
            .await
            .context("failed to parse chat.stopStream response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            bail!("chat.stopStream failed for channel {channel_id}: {error}");
        }

        Ok(())
    }
}

pub struct SlackProvider {
    bot_token: String,
    app_token: String,
    client: reqwest::Client,
    bot_user_id: String,
    user_cache: tokio::sync::RwLock<HashMap<String, String>>,
}

impl SlackProvider {
    /// Create a new Slack provider. Validates tokens via `auth.test`.
    pub async fn new(bot_token: &str, app_token: &str) -> Result<Self> {
        let client = reqwest::Client::new();

        let resp: serde_json::Value = client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(bot_token)
            .send()
            .await
            .context("failed to call auth.test")?
            .json()
            .await
            .context("failed to parse auth.test response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            bail!(
                "slack auth.test failed: {}",
                resp["error"].as_str().unwrap_or("unknown")
            );
        }

        let bot_user_id = resp["user_id"]
            .as_str()
            .context("auth.test missing user_id")?
            .to_string();

        let bot_name = resp["user"].as_str().unwrap_or("unknown");
        info!(bot_user_id, bot_name, "slack bot connected");

        Ok(Self {
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            client,
            bot_user_id,
            user_cache: tokio::sync::RwLock::new(HashMap::new()),
        })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> SlackOutbound {
        SlackOutbound {
            bot_token: self.bot_token.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the Socket Mode inbound dispatcher. Reconnects automatically on disconnect.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        loop {
            match self.run_socket_mode(&tx).await {
                Ok(()) => {
                    info!("socket mode connection closed, reconnecting...");
                }
                Err(e) => {
                    error!(error = %e, "socket mode error, reconnecting in 5s...");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn run_socket_mode(&self, tx: &mpsc::Sender<ConnectorEvent>) -> Result<()> {
        // Get a fresh WebSocket URL
        let resp: serde_json::Value = self
            .client
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.app_token)
            .send()
            .await
            .context("failed to call apps.connections.open")?
            .json()
            .await
            .context("failed to parse apps.connections.open response")?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            bail!(
                "apps.connections.open failed: {}",
                resp["error"].as_str().unwrap_or("unknown")
            );
        }

        let wss_url = resp["url"]
            .as_str()
            .context("apps.connections.open missing url")?;

        info!("connecting to slack socket mode");

        let (ws_stream, _) = connect_async(wss_url)
            .await
            .context("failed to connect to slack socket mode websocket")?;

        let (mut write, mut read) = ws_stream.split();

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    error!(error = %e, "websocket read error");
                    break;
                }
            };

            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(data) => {
                    let _ = write.send(Message::Pong(data)).await;
                    continue;
                }
                Message::Close(_) => {
                    info!("websocket closed by server");
                    break;
                }
                _ => continue,
            };

            let envelope: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "failed to parse socket mode envelope");
                    continue;
                }
            };

            // Acknowledge the envelope immediately
            if let Some(envelope_id) = envelope["envelope_id"].as_str() {
                let ack = serde_json::json!({"envelope_id": envelope_id});
                let _ = write.send(Message::Text(ack.to_string().into())).await;
            }

            let envelope_type = envelope["type"].as_str().unwrap_or("");

            match envelope_type {
                "hello" => {
                    info!("slack socket mode connected");
                }
                "disconnect" => {
                    info!("slack requested disconnect, will reconnect");
                    break;
                }
                "events_api" => {
                    self.handle_event(&envelope["payload"], tx).await;
                }
                other => {
                    debug!(envelope_type = other, "ignoring socket mode envelope");
                }
            }
        }

        Ok(())
    }

    async fn handle_event(&self, payload: &serde_json::Value, tx: &mpsc::Sender<ConnectorEvent>) {
        let event = &payload["event"];
        let event_type = event["type"].as_str().unwrap_or("");

        match event_type {
            "message" => {
                // Skip bot messages and message subtypes (edits, deletes, etc.)
                if event.get("bot_id").is_some() || event.get("subtype").is_some() {
                    debug!("skipping bot/subtype message");
                    return;
                }

                let user = match event["user"].as_str() {
                    Some(u) => u,
                    None => return,
                };

                // Skip our own messages
                if user == self.bot_user_id {
                    return;
                }

                let channel_id = match event["channel"].as_str() {
                    Some(c) => c,
                    None => return,
                };

                let text = event["text"].as_str().unwrap_or_default().to_string();
                if text.is_empty() {
                    return;
                }

                let ts = event["ts"].as_str().unwrap_or("0");
                let timestamp = parse_slack_ts(ts);

                // thread_ts indicates a threaded reply
                let thread_id = event["thread_ts"].as_str().map(|s| s.to_string());

                let sender_name = self
                    .resolve_user_name(user)
                    .await
                    .unwrap_or_else(|| user.to_string());

                let raw = InboundRaw {
                    chat_id: channel_id.to_string(),
                    sender_name,
                    text,
                    timestamp,
                    thread_id,
                    meta: serde_json::json!({
                        "channel_id": channel_id,
                        "message_ts": ts,
                        "user_id": user,
                        "team_id": event["team"].as_str().unwrap_or_default(),
                    }),
                };

                if let Err(e) = tx.send(ConnectorEvent::Message(raw)).await {
                    error!(error = %e, "failed to send slack inbound message to processor");
                }
            }
            "member_joined_channel" => {
                let user = event["user"].as_str().unwrap_or("");
                if user != self.bot_user_id {
                    return;
                }

                let channel_id = match event["channel"].as_str() {
                    Some(c) => c,
                    None => return,
                };

                info!(channel_id, "bot added to slack channel");

                if let Err(e) = tx
                    .send(ConnectorEvent::NewChat {
                        chat_id: channel_id.to_string(),
                        chat_title: None,
                    })
                    .await
                {
                    error!(error = %e, "failed to send new chat event");
                }
            }
            other => {
                debug!(event_type = other, "ignoring slack event");
            }
        }
    }

    /// Resolve a Slack user ID to a display name, with caching.
    async fn resolve_user_name(&self, user_id: &str) -> Option<String> {
        // Check cache first
        {
            let cache = self.user_cache.read().await;
            if let Some(name) = cache.get(user_id) {
                return Some(name.clone());
            }
        }

        // Fetch from API
        let resp: serde_json::Value = self
            .client
            .get("https://slack.com/api/users.info")
            .bearer_auth(&self.bot_token)
            .query(&[("user", user_id)])
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        if resp["ok"].as_bool().unwrap_or(false) {
            let user = &resp["user"];
            let name = user["profile"]["display_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| user["real_name"].as_str())
                .or_else(|| user["name"].as_str())?;
            let name = name.to_string();

            self.user_cache
                .write()
                .await
                .insert(user_id.to_string(), name.clone());

            Some(name)
        } else {
            None
        }
    }
}

fn extract_channel_id(msg: &OutboundMessage) -> Result<String> {
    msg.meta
        .get("channel_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("outbound message missing meta.channel_id")
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}

/// Parse Slack timestamp (e.g. "1234567890.123456") to Unix seconds.
fn parse_slack_ts(ts: &str) -> u64 {
    ts.split('.')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slack_ts_normal() {
        assert_eq!(parse_slack_ts("1700000000.123456"), 1700000000);
    }

    #[test]
    fn parse_slack_ts_no_decimal() {
        assert_eq!(parse_slack_ts("1700000000"), 1700000000);
    }

    #[test]
    fn parse_slack_ts_empty() {
        assert_eq!(parse_slack_ts(""), 0);
    }

    #[test]
    fn parse_slack_ts_invalid() {
        assert_eq!(parse_slack_ts("not-a-number"), 0);
    }
}
