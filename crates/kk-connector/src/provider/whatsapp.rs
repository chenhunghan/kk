use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const WHATSAPP_MAX_MESSAGE_LEN: usize = 4096;
const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v21.0";

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Handle for sending outbound messages via WhatsApp Cloud API.
#[derive(Clone)]
pub struct WhatsappOutbound {
    token: String,
    phone_number_id: String,
    client: reqwest::Client,
}

#[async_trait]
impl ChatProvider for WhatsappOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let to = extract_recipient(msg)?;

        let chunks = split_text(&msg.text, WHATSAPP_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let body = serde_json::json!({
                "messaging_product": "whatsapp",
                "to": to,
                "type": "text",
                "text": { "body": chunk },
            });

            let resp: serde_json::Value = self
                .client
                .post(format!(
                    "{WHATSAPP_API_BASE}/{}/messages",
                    self.phone_number_id
                ))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("failed to send whatsapp message to {to}"))?
                .json()
                .await
                .context("failed to parse whatsapp send response")?;

            // The Cloud API returns an error object when the request fails.
            if let Some(err) = resp.get("error") {
                let message = err["message"].as_str().unwrap_or("unknown");
                bail!("whatsapp send failed for {to}: {message}");
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let to = extract_recipient(msg)?;

        let text = truncate_text(&msg.text, WHATSAPP_MAX_MESSAGE_LEN);
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": to,
            "type": "text",
            "text": { "body": text },
        });

        let resp: serde_json::Value = self
            .client
            .post(format!(
                "{WHATSAPP_API_BASE}/{}/messages",
                self.phone_number_id
            ))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to send whatsapp message to {to}"))?
            .json()
            .await
            .context("failed to parse whatsapp send response")?;

        if let Some(err) = resp.get("error") {
            let message = err["message"].as_str().unwrap_or("unknown");
            bail!("whatsapp send failed for {to}: {message}");
        }

        let wamid = resp["messages"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|m| m["id"].as_str())
            .context("missing messages[0].id in whatsapp send response")?;

        Ok(wamid.to_string())
    }

    async fn edit(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        bail!("WhatsApp does not support message editing")
    }

    fn supports_native_stream(&self) -> bool {
        false
    }

    fn supports_edit(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Provider (inbound webhook)
// ---------------------------------------------------------------------------

/// WhatsApp Cloud API provider — runs an axum webhook server for inbound
/// messages and provides an outbound sender.
pub struct WhatsappProvider {
    token: String,
    phone_number_id: String,
    client: reqwest::Client,
    webhook_port: u16,
    verify_token: String,
}

impl WhatsappProvider {
    /// Create a new WhatsApp provider. Validates token by fetching phone number metadata.
    pub async fn new(
        token: &str,
        phone_number_id: &str,
        webhook_port: u16,
        verify_token: &str,
    ) -> Result<Self> {
        let client = reqwest::Client::new();

        // Validate token by fetching phone number info
        let resp: serde_json::Value = client
            .get(format!("{WHATSAPP_API_BASE}/{phone_number_id}"))
            .bearer_auth(token)
            .send()
            .await
            .context("failed to validate whatsapp token")?
            .json()
            .await
            .context("failed to parse whatsapp phone number response")?;

        if let Some(err) = resp.get("error") {
            let message = err["message"].as_str().unwrap_or("unknown");
            bail!("whatsapp token validation failed: {message}");
        }

        let display_name = resp["verified_name"].as_str().unwrap_or("unknown");

        info!(
            phone_number_id,
            display_name, webhook_port, "whatsapp provider connected"
        );

        Ok(Self {
            token: token.to_string(),
            phone_number_id: phone_number_id.to_string(),
            client,
            webhook_port,
            verify_token: verify_token.to_string(),
        })
    }

    /// Returns the phone number ID used for sending.
    pub fn phone_number_id(&self) -> &str {
        &self.phone_number_id
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> WhatsappOutbound {
        WhatsappOutbound {
            token: self.token.clone(),
            phone_number_id: self.phone_number_id.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the inbound webhook server. This blocks forever.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let state = Arc::new(WebhookState {
            tx,
            verify_token: self.verify_token.clone(),
        });

        let app = Router::new()
            .route("/webhook", get(handle_verify))
            .route("/webhook", post(handle_webhook))
            .route("/healthz", get(handle_health))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        info!(addr, "starting whatsapp webhook server");

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, addr, "failed to bind whatsapp webhook server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "whatsapp webhook server error");
        }
    }
}

// ---------------------------------------------------------------------------
// Axum state & handlers
// ---------------------------------------------------------------------------

struct WebhookState {
    tx: mpsc::Sender<ConnectorEvent>,
    verify_token: String,
}

/// Query parameters for the GET /webhook verification handshake.
#[derive(serde::Deserialize)]
struct VerifyQuery {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

/// GET /webhook — Meta webhook verification.
async fn handle_verify(
    State(state): State<Arc<WebhookState>>,
    Query(query): Query<VerifyQuery>,
) -> Result<String, StatusCode> {
    let mode = query.mode.as_deref().unwrap_or_default();
    let token = query.verify_token.as_deref().unwrap_or_default();
    let challenge = query.challenge.unwrap_or_default();

    if mode == "subscribe" && token == state.verify_token {
        info!("whatsapp webhook verification succeeded");
        Ok(challenge)
    } else {
        warn!(mode, "whatsapp webhook verification failed");
        Err(StatusCode::FORBIDDEN)
    }
}

/// POST /webhook — Incoming WhatsApp Cloud API webhook event.
async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    debug!("whatsapp webhook payload received");

    let entries = match body["entry"].as_array() {
        Some(arr) => arr,
        None => {
            debug!("whatsapp webhook: no entry array");
            return StatusCode::OK;
        }
    };

    for entry in entries {
        let changes = match entry["changes"].as_array() {
            Some(arr) => arr,
            None => continue,
        };

        for change in changes {
            let value = &change["value"];

            // Extract phone_number_id from metadata
            let phone_number_id = value["metadata"]["phone_number_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            // Build a contact name lookup: phone -> profile name
            let contacts = value["contacts"].as_array();
            let contact_name = |wa_id: &str| -> String {
                contacts
                    .and_then(|arr| arr.iter().find(|c| c["wa_id"].as_str() == Some(wa_id)))
                    .and_then(|c| c["profile"]["name"].as_str())
                    .unwrap_or(wa_id)
                    .to_string()
            };

            let messages = match value["messages"].as_array() {
                Some(arr) => arr,
                None => {
                    // Status updates or other non-message payloads — skip.
                    debug!("whatsapp webhook: no messages in change value, skipping");
                    continue;
                }
            };

            for message in messages {
                let msg_type = message["type"].as_str().unwrap_or_default();
                if msg_type != "text" {
                    debug!(msg_type, "skipping non-text whatsapp message");
                    continue;
                }

                let from = message["from"].as_str().unwrap_or_default();
                let text = message["text"]["body"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let message_id = message["id"].as_str().unwrap_or_default().to_string();
                let timestamp: u64 = message["timestamp"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);

                if text.is_empty() {
                    debug!(from, "skipping empty whatsapp text message");
                    continue;
                }

                let sender_name = contact_name(from);

                let raw = InboundRaw {
                    chat_id: from.to_string(),
                    sender_name,
                    text,
                    timestamp,
                    thread_id: None,
                    meta: serde_json::json!({
                        "phone_number_id": phone_number_id,
                        "message_id": message_id,
                        "from": from,
                    }),
                };

                if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
                    error!(error = %e, "failed to send whatsapp inbound message to processor");
                }
            }
        }
    }

    StatusCode::OK
}

/// GET /healthz — Liveness check.
async fn handle_health() -> StatusCode {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the recipient phone number from outbound message meta.
fn extract_recipient(msg: &OutboundMessage) -> Result<String> {
    msg.meta
        .get("to")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("outbound message missing meta.to")
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}
