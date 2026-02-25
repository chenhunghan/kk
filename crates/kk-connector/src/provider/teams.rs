use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::{RwLock, mpsc};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const TEAMS_MAX_MESSAGE_LEN: usize = 4000;
const TOKEN_URL: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";

/// Buffer before token expiry to trigger a refresh (5 minutes).
const TOKEN_REFRESH_BUFFER_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

/// Cached OAuth2 Bearer token for Bot Framework API calls.
struct TokenCache {
    token: String,
    expires_at: Instant,
}

impl TokenCache {
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Fetch a fresh OAuth2 token from the Bot Framework token endpoint.
async fn fetch_token(
    client: &reqwest::Client,
    app_id: &str,
    app_password: &str,
) -> Result<TokenCache> {
    let params = [
        ("grant_type", "client_credentials"),
        ("client_id", app_id),
        ("client_secret", app_password),
        ("scope", "https://api.botframework.com/.default"),
    ];

    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("failed to request bot framework token")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("bot framework token request failed: {status} {body}");
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse bot framework token response")?;

    let access_token = json["access_token"]
        .as_str()
        .context("missing access_token in token response")?
        .to_string();

    let expires_in = json["expires_in"].as_u64().unwrap_or(3600);

    // Subtract the refresh buffer so we refresh before actual expiry.
    let effective_lifetime = expires_in.saturating_sub(TOKEN_REFRESH_BUFFER_SECS);

    Ok(TokenCache {
        token: access_token,
        expires_at: Instant::now() + std::time::Duration::from_secs(effective_lifetime),
    })
}

/// Ensure we have a valid token, refreshing if expired.
async fn ensure_token(
    client: &reqwest::Client,
    app_id: &str,
    app_password: &str,
    cache: &RwLock<TokenCache>,
) -> Result<String> {
    // Fast path: read lock
    {
        let c = cache.read().await;
        if !c.is_expired() {
            return Ok(c.token.clone());
        }
    }

    // Slow path: write lock + refresh
    let mut c = cache.write().await;
    // Double-check after acquiring write lock (another task may have refreshed).
    if !c.is_expired() {
        return Ok(c.token.clone());
    }

    info!("refreshing bot framework token");
    let new_cache = fetch_token(client, app_id, app_password).await?;
    *c = new_cache;
    Ok(c.token.clone())
}

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Handle for sending outbound messages via the Bot Framework REST API.
#[derive(Clone)]
pub struct TeamsOutbound {
    app_id: String,
    app_password: String,
    client: reqwest::Client,
    token_cache: Arc<RwLock<TokenCache>>,
}

#[async_trait]
impl ChatProvider for TeamsOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let service_url = extract_service_url(msg)?;
        let conversation_id = extract_conversation_id(msg)?;

        let chunks = split_text(&msg.text, TEAMS_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let token = ensure_token(
                &self.client,
                &self.app_id,
                &self.app_password,
                &self.token_cache,
            )
            .await?;

            let url = format!("{service_url}v3/conversations/{conversation_id}/activities");

            let mut body = serde_json::json!({
                "type": "message",
                "text": chunk,
            });

            // Thread reply if we have a replyToId
            if let Some(ref thread_id) = msg.thread_id {
                body["replyToId"] = serde_json::Value::String(thread_id.clone());
            }

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&token)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .with_context(|| {
                    format!("failed to send teams message to conversation {conversation_id}")
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                bail!(
                    "teams send message failed for conversation {conversation_id}: {status} {text}"
                );
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let service_url = extract_service_url(msg)?;
        let conversation_id = extract_conversation_id(msg)?;

        let token = ensure_token(
            &self.client,
            &self.app_id,
            &self.app_password,
            &self.token_cache,
        )
        .await?;

        let text = truncate_text(&msg.text, TEAMS_MAX_MESSAGE_LEN);
        let url = format!("{service_url}v3/conversations/{conversation_id}/activities");

        let mut body = serde_json::json!({
            "type": "message",
            "text": text,
        });

        if let Some(ref thread_id) = msg.thread_id {
            body["replyToId"] = serde_json::Value::String(thread_id.clone());
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to send teams message to conversation {conversation_id}")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("teams send message failed for conversation {conversation_id}: {status} {text}");
        }

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse teams send message response")?;

        let activity_id = resp_json["id"]
            .as_str()
            .context("missing id in teams send message response")?;

        Ok(activity_id.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let service_url = extract_service_url(msg)?;
        let conversation_id = extract_conversation_id(msg)?;

        let token = ensure_token(
            &self.client,
            &self.app_id,
            &self.app_password,
            &self.token_cache,
        )
        .await?;

        let text = truncate_text(&msg.text, TEAMS_MAX_MESSAGE_LEN);
        let url =
            format!("{service_url}v3/conversations/{conversation_id}/activities/{platform_msg_id}");

        let body = serde_json::json!({
            "type": "message",
            "text": text,
        });

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to edit teams message {platform_msg_id} in conversation {conversation_id}"
                )
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "teams edit message failed for conversation {conversation_id} activity {platform_msg_id}: {status} {text}"
            );
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
    bot_app_id: String,
}

/// Microsoft Teams provider: authenticates via Bot Framework OAuth2, runs
/// webhook server for inbound activities.
pub struct TeamsProvider {
    app_id: String,
    app_password: String,
    client: reqwest::Client,
    webhook_port: u16,
    token_cache: Arc<RwLock<TokenCache>>,
}

impl TeamsProvider {
    /// Create a new Teams provider. Fetches an initial token to validate credentials.
    pub async fn new(app_id: &str, app_password: &str, webhook_port: u16) -> Result<Self> {
        let client = reqwest::Client::new();

        // Validate credentials by fetching the first token.
        let initial_cache = fetch_token(&client, app_id, app_password).await.context(
            "failed to authenticate with bot framework — check TEAMS_APP_ID and TEAMS_APP_PASSWORD",
        )?;

        info!(app_id, "teams bot authenticated");

        Ok(Self {
            app_id: app_id.to_string(),
            app_password: app_password.to_string(),
            client,
            webhook_port,
            token_cache: Arc::new(RwLock::new(initial_cache)),
        })
    }

    /// Returns the bot's app/client ID.
    pub fn bot_app_id(&self) -> &str {
        &self.app_id
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> TeamsOutbound {
        TeamsOutbound {
            app_id: self.app_id.clone(),
            app_password: self.app_password.clone(),
            client: self.client.clone(),
            token_cache: Arc::clone(&self.token_cache),
        }
    }

    /// Start the webhook HTTP server for inbound Bot Framework activities. Runs forever.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let state = Arc::new(WebhookState {
            tx,
            bot_app_id: self.app_id.clone(),
        });

        let app = Router::new()
            .route("/api/messages", post(handle_activity))
            .route("/healthz", get(handle_healthz))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        info!(addr, "starting teams webhook server");

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, addr, "failed to bind teams webhook server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "teams webhook server error");
        }
    }
}

/// Health check endpoint.
async fn handle_healthz() -> StatusCode {
    StatusCode::OK
}

/// Webhook handler: receives Bot Framework activity payloads and dispatches events.
async fn handle_activity(
    State(state): State<Arc<WebhookState>>,
    Json(activity): Json<serde_json::Value>,
) -> StatusCode {
    // TODO: Validate JWT token from Authorization header.
    // The Bot Framework sends a Bearer token in the Authorization header that should
    // be validated against Microsoft's OpenID metadata to ensure the request is
    // authentic. For now we accept all payloads.

    let activity_type = activity["type"].as_str().unwrap_or("");

    if activity_type != "message" {
        debug!(activity_type, "ignoring non-message teams activity");
        return StatusCode::OK;
    }

    let from_id = match activity["from"]["id"].as_str() {
        Some(id) => id,
        None => {
            warn!("teams activity missing from.id");
            return StatusCode::OK;
        }
    };

    // Skip messages from the bot itself.
    if from_id == state.bot_app_id {
        debug!("skipping own teams message");
        return StatusCode::OK;
    }

    let text = activity["text"].as_str().unwrap_or_default().to_string();
    if text.is_empty() {
        return StatusCode::OK;
    }

    let conversation_id = match activity["conversation"]["id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            warn!("teams activity missing conversation.id");
            return StatusCode::OK;
        }
    };

    let service_url = match activity["serviceUrl"].as_str() {
        Some(url) => url.to_string(),
        None => {
            warn!("teams activity missing serviceUrl");
            return StatusCode::OK;
        }
    };

    // Ensure service_url ends with '/' for consistent URL construction.
    let service_url = if service_url.ends_with('/') {
        service_url
    } else {
        format!("{service_url}/")
    };

    let sender_name = activity["from"]["name"]
        .as_str()
        .unwrap_or(from_id)
        .to_string();

    let activity_id = activity["id"].as_str().unwrap_or_default().to_string();
    let reply_to_id = activity["replyToId"].as_str().map(|s| s.to_string());
    let tenant_id = activity["conversation"]["tenantId"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let timestamp = parse_teams_timestamp(activity["timestamp"].as_str().unwrap_or_default());

    debug!(
        conversation_id,
        sender_name, activity_id, "received teams message"
    );

    let raw = InboundRaw {
        chat_id: conversation_id.clone(),
        sender_name,
        text,
        timestamp,
        thread_id: reply_to_id,
        meta: serde_json::json!({
            "service_url": service_url,
            "conversation_id": conversation_id,
            "activity_id": activity_id,
            "from_id": from_id,
            "tenant_id": tenant_id,
        }),
    };

    if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
        error!(error = %e, "failed to send teams inbound message to processor");
    }

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract `service_url` from the outbound message meta.
fn extract_service_url(msg: &OutboundMessage) -> Result<String> {
    let url = msg
        .meta
        .get("service_url")
        .and_then(|v| v.as_str())
        .context("outbound message missing meta.service_url")?
        .to_string();

    // Ensure trailing slash for consistent URL building.
    if url.ends_with('/') {
        Ok(url)
    } else {
        Ok(format!("{url}/"))
    }
}

/// Extract `conversation_id` from the outbound message meta.
fn extract_conversation_id(msg: &OutboundMessage) -> Result<String> {
    msg.meta
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("outbound message missing meta.conversation_id")
}

/// Truncate text to fit within the platform's max message length.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}

/// Parse a Bot Framework ISO 8601 timestamp (e.g. "2024-01-15T12:34:56.123Z") to Unix seconds.
fn parse_teams_timestamp(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0)
}

/// Sanitize a Teams conversation ID into an auto-registration slug.
///
/// Replaces non-alphanumeric characters with `-`, lowercases, and truncates to 60 chars.
/// Result is prefixed with `teams-`.
pub fn auto_registration_slug(conversation_id: &str) -> String {
    let sanitized: String = conversation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    let truncated = if sanitized.len() > 60 {
        &sanitized[..60]
    } else {
        &sanitized
    };

    format!("teams-{truncated}")
}
