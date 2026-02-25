use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const GCHAT_MAX_MESSAGE_LEN: usize = 4096;
const GCHAT_API_BASE: &str = "https://chat.googleapis.com/v1";

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Handle for sending outbound messages via Google Chat REST API.
#[derive(Clone)]
pub struct GchatOutbound {
    token: String,
    client: reqwest::Client,
}

impl GchatOutbound {
    pub fn new(token: String, client: reqwest::Client) -> Self {
        Self { token, client }
    }
}

#[async_trait]
impl ChatProvider for GchatOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let space_name = extract_space_name(msg)?;

        let chunks = split_text(&msg.text, GCHAT_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut url = format!("{GCHAT_API_BASE}/{space_name}/messages");
            let mut body = serde_json::json!({ "text": chunk });

            // Thread the reply if thread_id is set
            if let Some(ref thread_name) = msg.thread_id {
                url = format!("{url}?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
                body["thread"] = serde_json::json!({ "name": thread_name });
            } else if let Some(thread_name) = msg.meta.get("thread_name").and_then(|v| v.as_str()) {
                url = format!("{url}?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
                body["thread"] = serde_json::json!({ "name": thread_name });
            }

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("failed to send gchat message to {space_name}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                bail!("gchat create message failed for {space_name}: {status} {text}");
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let space_name = extract_space_name(msg)?;

        let text = truncate_text(&msg.text, GCHAT_MAX_MESSAGE_LEN);
        let mut url = format!("{GCHAT_API_BASE}/{space_name}/messages");
        let mut body = serde_json::json!({ "text": text });

        if let Some(ref thread_name) = msg.thread_id {
            url = format!("{url}?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
            body["thread"] = serde_json::json!({ "name": thread_name });
        } else if let Some(thread_name) = msg.meta.get("thread_name").and_then(|v| v.as_str()) {
            url = format!("{url}?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
            body["thread"] = serde_json::json!({ "name": thread_name });
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to send gchat message to {space_name}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("gchat create message failed for {space_name}: {status} {text}");
        }

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse gchat create message response")?;

        let message_name = resp_json["name"]
            .as_str()
            .context("missing name in gchat create message response")?;

        Ok(message_name.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        // platform_msg_id is the full message name (e.g. "spaces/AAAA1234/messages/MSG_ID")
        // Alternatively, construct from meta if platform_msg_id is just the message portion.
        let message_name = if platform_msg_id.starts_with("spaces/") {
            platform_msg_id.to_string()
        } else {
            // Fall back to constructing from meta.space_name + platform_msg_id
            let space_name = extract_space_name(msg)?;
            format!("{space_name}/messages/{platform_msg_id}")
        };

        let text = truncate_text(&msg.text, GCHAT_MAX_MESSAGE_LEN);
        let url = format!("{GCHAT_API_BASE}/{message_name}?updateMask=text");
        let body = serde_json::json!({ "text": text });

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to edit gchat message {message_name}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("gchat edit message failed for {message_name}: {status} {text}");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Inbound (webhook server)
// ---------------------------------------------------------------------------

/// Shared state for the axum webhook handler.
#[derive(Clone)]
struct WebhookState {
    tx: mpsc::Sender<ConnectorEvent>,
}

/// Google Chat provider: validates token, runs webhook server for inbound events.
pub struct GchatProvider {
    token: String,
    client: reqwest::Client,
    webhook_port: u16,
}

impl GchatProvider {
    /// Create a new Google Chat provider. Validates the token by listing spaces.
    pub async fn new(token: &str, webhook_port: u16) -> Result<Self> {
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("{GCHAT_API_BASE}/spaces?pageSize=1"))
            .bearer_auth(token)
            .send()
            .await
            .context("failed to call GET /v1/spaces for token validation")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("gchat token validation failed: {status} {body}");
        }

        info!("gchat bot connected");

        Ok(Self {
            token: token.to_string(),
            client,
            webhook_port,
        })
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> GchatOutbound {
        GchatOutbound {
            token: self.token.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the webhook HTTP server for inbound events. Runs forever.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let state = Arc::new(WebhookState { tx });

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .route("/healthz", get(handle_healthz))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        info!(addr, "starting gchat webhook server");

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, addr, "failed to bind gchat webhook server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "gchat webhook server error");
        }
    }
}

/// Health check endpoint.
async fn handle_healthz() -> StatusCode {
    StatusCode::OK
}

/// Webhook handler: receives Google Chat event payloads and dispatches events.
async fn handle_webhook(State(state): State<Arc<WebhookState>>, body: String) -> StatusCode {
    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse gchat webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    let event_type = payload["type"].as_str().unwrap_or("");
    debug!(event_type, "received gchat webhook event");

    match event_type {
        "MESSAGE" => {
            handle_message(&state, &payload).await;
        }
        "ADDED_TO_SPACE" => {
            handle_added_to_space(&state, &payload).await;
        }
        _ => {
            debug!(event_type, "ignoring unhandled gchat event type");
        }
    }

    StatusCode::OK
}

/// Handle `MESSAGE` event: a user sent a message in a space.
async fn handle_message(state: &WebhookState, payload: &serde_json::Value) {
    let message = &payload["message"];

    // Skip bot messages
    let sender_type = message["sender"]["type"].as_str().unwrap_or("");
    if sender_type == "BOT" {
        debug!("skipping bot message");
        return;
    }

    // Prefer argumentText (bot mention stripped) over text
    let text = message["argumentText"]
        .as_str()
        .or_else(|| message["text"].as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    if text.is_empty() {
        return;
    }

    let sender_name = message["sender"]["displayName"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    let space_name = message["space"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let message_name = message["name"].as_str().unwrap_or_default().to_string();

    let thread_name = message["thread"]["name"].as_str().map(|s| s.to_string());

    let timestamp = parse_gchat_timestamp(message["createTime"].as_str().unwrap_or_default());

    let chat_id = space_name.clone();

    let raw = InboundRaw {
        chat_id,
        sender_name,
        text,
        timestamp,
        thread_id: thread_name.clone(),
        meta: serde_json::json!({
            "space_name": space_name,
            "message_name": message_name,
            "thread_name": thread_name.unwrap_or_default(),
        }),
    };

    if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
        error!(error = %e, "failed to send gchat inbound message to processor");
    }
}

/// Handle `ADDED_TO_SPACE` event: bot was added to a space.
async fn handle_added_to_space(state: &WebhookState, payload: &serde_json::Value) {
    let space = &payload["space"];
    let space_name = space["name"].as_str().unwrap_or_default().to_string();

    if space_name.is_empty() {
        warn!("ADDED_TO_SPACE event missing space.name");
        return;
    }

    let space_display_name = space["displayName"].as_str().map(|s| s.to_string());
    info!(space_name, ?space_display_name, "bot added to gchat space");

    if let Err(e) = state
        .tx
        .send(ConnectorEvent::NewChat {
            chat_id: space_name,
            chat_title: space_display_name,
        })
        .await
    {
        error!(error = %e, "failed to send gchat new chat event");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract `space_name` from the outbound message meta.
fn extract_space_name(msg: &OutboundMessage) -> Result<String> {
    msg.meta
        .get("space_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("outbound message missing meta.space_name")
}

/// Truncate text to fit within the platform's max message length.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}

/// Extract a slug for auto-registration from a space name.
/// `spaces/AAAA1234` -> `gchat-aaaa1234`
pub fn space_name_to_slug(space_name: &str) -> String {
    let space_id = space_name
        .strip_prefix("spaces/")
        .unwrap_or(space_name)
        .to_lowercase();
    format!("gchat-{space_id}")
}

/// Parse a Google Chat RFC 3339 timestamp (e.g. "2024-01-15T12:00:00.000000Z") to Unix seconds.
fn parse_gchat_timestamp(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0)
}
