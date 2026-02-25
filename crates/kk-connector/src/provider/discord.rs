use std::sync::atomic::{AtomicI64, Ordering};

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

const DISCORD_MAX_MESSAGE_LEN: usize = 2000;
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord Gateway opcodes.
const OP_DISPATCH: u64 = 0;
const OP_HEARTBEAT: u64 = 1;
const OP_IDENTIFY: u64 = 2;
const OP_RESUME: u64 = 6;
const OP_RECONNECT: u64 = 7;
const OP_INVALID_SESSION: u64 = 9;
const OP_HELLO: u64 = 10;
const OP_HEARTBEAT_ACK: u64 = 11;

/// Gateway intents: GUILD_MESSAGES (1 << 9) | MESSAGE_CONTENT (1 << 15) = 33280
const GATEWAY_INTENTS: u64 = 33280;

/// Handle for sending outbound messages via the Discord REST API.
#[derive(Clone)]
pub struct DiscordOutbound {
    bot_token: String,
    client: reqwest::Client,
}

impl DiscordOutbound {
    pub fn new(bot_token: String, client: reqwest::Client) -> Self {
        Self { bot_token, client }
    }
}

#[async_trait]
impl ChatProvider for DiscordOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let chunks = split_text(&msg.text, DISCORD_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut body = serde_json::json!({
                "content": chunk,
            });

            // Thread reply via message_reference (only on first chunk)
            if i == 0
                && let Some(ref thread_id) = msg.thread_id
            {
                body["message_reference"] = serde_json::json!({
                    "message_id": thread_id,
                });
            }

            let resp = self
                .client
                .post(format!("{DISCORD_API_BASE}/channels/{channel_id}/messages"))
                .header("Authorization", format!("Bot {}", self.bot_token))
                .json(&body)
                .send()
                .await
                .with_context(|| {
                    format!("failed to send discord message to channel {channel_id}")
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                bail!("discord send failed for channel {channel_id}: {status} {text}");
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, DISCORD_MAX_MESSAGE_LEN);
        let mut body = serde_json::json!({
            "content": text,
        });

        if let Some(ref thread_id) = msg.thread_id {
            body["message_reference"] = serde_json::json!({
                "message_id": thread_id,
            });
        }

        let resp = self
            .client
            .post(format!("{DISCORD_API_BASE}/channels/{channel_id}/messages"))
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to send discord message to channel {channel_id}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("discord send_returning_id failed for channel {channel_id}: {status} {text}");
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse discord send response")?;

        let message_id = data["id"]
            .as_str()
            .context("missing id in discord send response")?;
        Ok(message_id.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;

        let text = truncate_text(&msg.text, DISCORD_MAX_MESSAGE_LEN);
        let body = serde_json::json!({
            "content": text,
        });

        let resp = self
            .client
            .patch(format!(
                "{DISCORD_API_BASE}/channels/{channel_id}/messages/{platform_msg_id}"
            ))
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to edit discord message {platform_msg_id} in channel {channel_id}")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "discord edit failed for message {platform_msg_id} in channel {channel_id}: {status} {text}"
            );
        }

        Ok(())
    }
}

pub struct DiscordProvider {
    bot_token: String,
    client: reqwest::Client,
    bot_user_id: String,
}

impl DiscordProvider {
    /// Create a new Discord provider. Validates the bot token via the gateway/bot endpoint.
    pub async fn new(bot_token: &str) -> Result<Self> {
        let client = reqwest::Client::new();

        // Validate token by requesting gateway URL
        let resp = client
            .get(format!("{DISCORD_API_BASE}/gateway/bot"))
            .header("Authorization", format!("Bot {bot_token}"))
            .send()
            .await
            .context("failed to call discord gateway/bot")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("discord gateway/bot validation failed: {status} {text}");
        }

        let _gateway_data: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse gateway/bot response")?;

        // Also fetch current user to get bot_user_id
        let user_resp = client
            .get(format!("{DISCORD_API_BASE}/users/@me"))
            .header("Authorization", format!("Bot {bot_token}"))
            .send()
            .await
            .context("failed to call discord users/@me")?;

        if !user_resp.status().is_success() {
            let status = user_resp.status();
            let text = user_resp.text().await.unwrap_or_default();
            bail!("discord users/@me failed: {status} {text}");
        }

        let user_data: serde_json::Value = user_resp
            .json()
            .await
            .context("failed to parse users/@me response")?;

        let bot_user_id = user_data["id"]
            .as_str()
            .context("users/@me missing id")?
            .to_string();

        let bot_name = user_data["username"].as_str().unwrap_or("unknown");
        info!(bot_user_id, bot_name, "discord bot connected");

        Ok(Self {
            bot_token: bot_token.to_string(),
            client,
            bot_user_id,
        })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> DiscordOutbound {
        DiscordOutbound {
            bot_token: self.bot_token.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the Gateway WebSocket inbound dispatcher. Reconnects automatically on disconnect.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let mut resume_state: Option<ResumeState> = None;

        loop {
            match self.run_gateway(&tx, &mut resume_state).await {
                Ok(()) => {
                    info!("discord gateway connection closed, reconnecting...");
                }
                Err(e) => {
                    error!(error = %e, "discord gateway error, reconnecting in 5s...");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Get the Gateway WebSocket URL.
    async fn get_gateway_url(&self) -> Result<String> {
        let resp: serde_json::Value = self
            .client
            .get(format!("{DISCORD_API_BASE}/gateway/bot"))
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await
            .context("failed to call gateway/bot")?
            .json()
            .await
            .context("failed to parse gateway/bot response")?;

        let url = resp["url"].as_str().context("gateway/bot missing url")?;
        Ok(format!("{url}/?v=10&encoding=json"))
    }

    /// Run a single Gateway WebSocket connection lifecycle.
    async fn run_gateway(
        &self,
        tx: &mpsc::Sender<ConnectorEvent>,
        resume_state: &mut Option<ResumeState>,
    ) -> Result<()> {
        let wss_url = self.get_gateway_url().await?;
        info!("connecting to discord gateway");

        let (ws_stream, _) = connect_async(&wss_url)
            .await
            .context("failed to connect to discord gateway websocket")?;

        let (mut write, mut read) = ws_stream.split();

        // Wait for Hello (opcode 10)
        let heartbeat_interval_ms = loop {
            let msg = read
                .next()
                .await
                .context("gateway closed before Hello")?
                .context("websocket read error waiting for Hello")?;

            if let Message::Text(text) = msg {
                let payload: serde_json::Value =
                    serde_json::from_str(&text).context("failed to parse Hello payload")?;

                let op = payload["op"].as_u64().unwrap_or(0);
                if op == OP_HELLO {
                    let interval = payload["d"]["heartbeat_interval"]
                        .as_u64()
                        .context("Hello missing heartbeat_interval")?;
                    info!(heartbeat_interval_ms = interval, "received Hello");
                    break interval;
                }
            }
        };

        // Send Identify or Resume
        if let Some(state) = resume_state.as_ref() {
            info!(session_id = %state.session_id, "resuming discord gateway session");
            let resume_payload = serde_json::json!({
                "op": OP_RESUME,
                "d": {
                    "token": self.bot_token,
                    "session_id": state.session_id,
                    "seq": state.sequence.load(Ordering::SeqCst),
                }
            });
            write
                .send(Message::Text(resume_payload.to_string().into()))
                .await
                .context("failed to send Resume")?;
        } else {
            let identify_payload = serde_json::json!({
                "op": OP_IDENTIFY,
                "d": {
                    "token": self.bot_token,
                    "intents": GATEWAY_INTENTS,
                    "properties": {
                        "os": "linux",
                        "browser": "kk-connector",
                        "device": "kk-connector",
                    }
                }
            });
            write
                .send(Message::Text(identify_payload.to_string().into()))
                .await
                .context("failed to send Identify")?;
        }

        // Shared sequence number for heartbeat task
        let sequence = resume_state
            .as_ref()
            .map(|s| s.sequence.clone())
            .unwrap_or_else(|| std::sync::Arc::new(AtomicI64::new(-1)));

        // Channel for the heartbeat task to send heartbeat frames through
        let (hb_tx, mut hb_rx) = mpsc::channel::<serde_json::Value>(4);

        // Spawn heartbeat task
        let hb_sequence = sequence.clone();
        let hb_interval = heartbeat_interval_ms;
        let hb_handle = tokio::spawn(async move {
            // First heartbeat after jitter (random fraction of interval)
            let jitter_ms = hb_interval as f64 * rand_jitter();
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms as u64)).await;

            let mut interval = tokio::time::interval(std::time::Duration::from_millis(hb_interval));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let seq = hb_sequence.load(Ordering::SeqCst);
                let heartbeat = serde_json::json!({
                    "op": OP_HEARTBEAT,
                    "d": if seq < 0 { serde_json::Value::Null } else { serde_json::Value::Number(seq.into()) },
                });

                if hb_tx.send(heartbeat).await.is_err() {
                    break;
                }
                interval.tick().await;
            }
        });

        let mut session_id_captured: Option<String> =
            resume_state.as_ref().map(|s| s.session_id.clone());

        // Main read loop: alternate between reading WS messages and sending heartbeats
        loop {
            tokio::select! {
                ws_msg = read.next() => {
                    let ws_msg = match ws_msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            error!(error = %e, "discord websocket read error");
                            break;
                        }
                        None => {
                            info!("discord websocket stream ended");
                            break;
                        }
                    };

                    match ws_msg {
                        Message::Text(text) => {
                            let payload: serde_json::Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(error = %e, "failed to parse gateway payload");
                                    continue;
                                }
                            };

                            let op = payload["op"].as_u64().unwrap_or(0);

                            // Update sequence number for dispatch events
                            if let Some(s) = payload["s"].as_i64() {
                                sequence.store(s, Ordering::SeqCst);
                            }

                            match op {
                                OP_DISPATCH => {
                                    let event_name = payload["t"].as_str().unwrap_or("");
                                    self.handle_dispatch(event_name, &payload["d"], tx, &mut session_id_captured).await;
                                }
                                OP_HEARTBEAT => {
                                    // Server is requesting an immediate heartbeat
                                    let seq = sequence.load(Ordering::SeqCst);
                                    let hb = serde_json::json!({
                                        "op": OP_HEARTBEAT,
                                        "d": if seq < 0 { serde_json::Value::Null } else { serde_json::Value::Number(seq.into()) },
                                    });
                                    let _ = write.send(Message::Text(hb.to_string().into())).await;
                                }
                                OP_RECONNECT => {
                                    info!("discord gateway requested reconnect (opcode 7)");
                                    break;
                                }
                                OP_INVALID_SESSION => {
                                    let resumable = payload["d"].as_bool().unwrap_or(false);
                                    if !resumable {
                                        warn!("discord invalid session (not resumable), clearing resume state");
                                        *resume_state = None;
                                    } else {
                                        info!("discord invalid session (resumable)");
                                    }
                                    // Wait 1-5 seconds as recommended by Discord
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    break;
                                }
                                OP_HEARTBEAT_ACK => {
                                    debug!("heartbeat ack received");
                                }
                                other => {
                                    debug!(opcode = other, "ignoring gateway opcode");
                                }
                            }
                        }
                        Message::Close(frame) => {
                            info!(?frame, "discord websocket closed by server");
                            break;
                        }
                        _ => {}
                    }
                }
                Some(hb_payload) = hb_rx.recv() => {
                    if let Err(e) = write.send(Message::Text(hb_payload.to_string().into())).await {
                        error!(error = %e, "failed to send heartbeat");
                        break;
                    }
                    debug!("heartbeat sent");
                }
            }
        }

        hb_handle.abort();

        // Save resume state if we have a session_id
        if let Some(sid) = session_id_captured {
            *resume_state = Some(ResumeState {
                session_id: sid,
                sequence: sequence.clone(),
            });
        }

        Ok(())
    }

    /// Handle a dispatch event (opcode 0) from the Gateway.
    async fn handle_dispatch(
        &self,
        event_name: &str,
        data: &serde_json::Value,
        tx: &mpsc::Sender<ConnectorEvent>,
        session_id: &mut Option<String>,
    ) {
        match event_name {
            "READY" => {
                let sid = data["session_id"].as_str().unwrap_or_default();
                let user_id = data["user"]["id"].as_str().unwrap_or_default();
                info!(session_id = sid, user_id, "discord gateway READY");
                *session_id = Some(sid.to_string());
            }
            "RESUMED" => {
                info!("discord gateway session resumed");
            }
            "MESSAGE_CREATE" => {
                self.handle_message_create(data, tx).await;
            }
            "GUILD_CREATE" => {
                self.handle_guild_create(data, tx).await;
            }
            other => {
                debug!(event = other, "ignoring discord dispatch event");
            }
        }
    }

    /// Handle MESSAGE_CREATE dispatch.
    async fn handle_message_create(
        &self,
        data: &serde_json::Value,
        tx: &mpsc::Sender<ConnectorEvent>,
    ) {
        let author = &data["author"];

        // Skip bot messages
        if author["bot"].as_bool().unwrap_or(false) {
            debug!("skipping bot message");
            return;
        }

        let author_id = match author["id"].as_str() {
            Some(id) => id,
            None => return,
        };

        // Skip our own messages
        if author_id == self.bot_user_id {
            return;
        }

        let channel_id = match data["channel_id"].as_str() {
            Some(c) => c,
            None => return,
        };

        let content = data["content"].as_str().unwrap_or_default().to_string();
        if content.is_empty() {
            return;
        }

        let username = author["username"].as_str().unwrap_or("unknown");

        // Parse ISO 8601 timestamp to unix seconds
        let timestamp = data["timestamp"]
            .as_str()
            .and_then(parse_discord_timestamp)
            .unwrap_or(0);

        let message_id = data["id"].as_str().unwrap_or_default();
        let guild_id = data["guild_id"].as_str().unwrap_or_default();

        // Determine thread_id from message_reference (reply) if present
        let thread_id = data["message_reference"]
            .get("message_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let raw = InboundRaw {
            chat_id: channel_id.to_string(),
            sender_name: username.to_string(),
            text: content,
            timestamp,
            thread_id,
            meta: serde_json::json!({
                "channel_id": channel_id,
                "message_id": message_id,
                "guild_id": guild_id,
                "author_id": author_id,
            }),
        };

        if let Err(e) = tx.send(ConnectorEvent::Message(raw)).await {
            error!(error = %e, "failed to send discord inbound message to processor");
        }
    }

    /// Handle GUILD_CREATE dispatch - fires NewChat events for each text channel.
    async fn handle_guild_create(
        &self,
        data: &serde_json::Value,
        tx: &mpsc::Sender<ConnectorEvent>,
    ) {
        let guild_id = data["id"].as_str().unwrap_or("unknown");
        let guild_name = data["name"].as_str().unwrap_or("unknown");
        info!(guild_id, guild_name, "received GUILD_CREATE");

        // Fire NewChat for each text channel in the guild
        if let Some(channels) = data["channels"].as_array() {
            for channel in channels {
                // type 0 = GUILD_TEXT
                let channel_type = channel["type"].as_u64().unwrap_or(u64::MAX);
                if channel_type != 0 {
                    continue;
                }

                let channel_id = match channel["id"].as_str() {
                    Some(id) => id,
                    None => continue,
                };
                let channel_name = channel["name"].as_str();

                if let Err(e) = tx
                    .send(ConnectorEvent::NewChat {
                        chat_id: channel_id.to_string(),
                        chat_title: channel_name.map(|s| s.to_string()),
                    })
                    .await
                {
                    error!(error = %e, channel_id, "failed to send new chat event for guild channel");
                }
            }
        }
    }
}

/// State needed to resume a Gateway session after disconnect.
struct ResumeState {
    session_id: String,
    sequence: std::sync::Arc<AtomicI64>,
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

/// Parse a Discord ISO 8601 timestamp (e.g. "2023-11-14T12:34:56.789000+00:00") to Unix seconds.
fn parse_discord_timestamp(ts: &str) -> Option<u64> {
    // Simple parsing: look for the 'T' separator and parse the date/time parts.
    // Discord timestamps are always in ISO 8601 format.
    // We do a basic parse without pulling in chrono.
    let ts = ts.trim();
    if ts.len() < 19 {
        return None;
    }

    let date_time = &ts[..19]; // "2023-11-14T12:34:56"
    let parts: Vec<&str> = date_time.split('T').collect();
    if parts.len() != 2 {
        return None;
    }

    let date_parts: Vec<u64> = parts[0].split('-').filter_map(|s| s.parse().ok()).collect();
    let time_parts: Vec<u64> = parts[1].split(':').filter_map(|s| s.parse().ok()).collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }

    let (year, month, day) = (date_parts[0], date_parts[1], date_parts[2]);
    let (hour, minute, second) = (time_parts[0], time_parts[1], time_parts[2]);

    // Convert to Unix timestamp using a simplified calculation.
    // Days from epoch (1970-01-01) to the given date.
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86400 + hour * 3600 + minute * 60 + second;
    Some(secs)
}

/// Calculate days from Unix epoch (1970-01-01) to a given civil date.
/// Returns None for dates before epoch.
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days in each month (non-leap)
    let days_in_month: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let mut total_days: u64 = 0;

    // Full years from 1970
    for y in 1970..year {
        total_days += if is_leap_year(y) { 366 } else { 365 };
    }

    // Full months in the current year
    for m in 1..month {
        let idx = (m - 1) as usize;
        total_days += days_in_month[idx];
        if m == 2 && is_leap_year(year) {
            total_days += 1;
        }
    }

    // Days in current month
    total_days += day - 1;

    Some(total_days)
}

fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Generate a pseudo-random jitter factor between 0.0 and 1.0.
/// Uses the current time as a simple entropy source to avoid pulling in a rand crate.
fn rand_jitter() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Map nanoseconds to 0.0..1.0
    (now as f64) / 1_000_000_000.0
}
