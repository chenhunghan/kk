use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const LINEAR_MAX_MESSAGE_LEN: usize = 100_000;
const LINEAR_API_URL: &str = "https://api.linear.app/graphql";

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Handle for sending outbound messages via Linear GraphQL API.
#[derive(Clone)]
pub struct LinearOutbound {
    api_key: String,
    client: reqwest::Client,
}

impl LinearOutbound {
    pub fn new(api_key: String, client: reqwest::Client) -> Self {
        Self { api_key, client }
    }

    /// Execute a GraphQL request against the Linear API and return the JSON response.
    async fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(LINEAR_API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send linear graphql request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("linear graphql request failed: {status} {text}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse linear graphql response")?;

        if let Some(errors) = json.get("errors") {
            bail!("linear graphql errors: {errors}");
        }

        Ok(json)
    }
}

#[async_trait]
impl ChatProvider for LinearOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let issue_id = extract_issue_id(msg)?;

        let chunks = split_text(&msg.text, LINEAR_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let query = "mutation($issueId: String!, $body: String!) { \
                commentCreate(input: { issueId: $issueId, body: $body }) { \
                    success comment { id } \
                } \
            }";

            let variables = serde_json::json!({
                "issueId": issue_id,
                "body": chunk,
            });

            let resp = self
                .graphql(query, variables)
                .await
                .with_context(|| format!("failed to create linear comment on issue {issue_id}"))?;

            let success = resp
                .pointer("/data/commentCreate/success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !success {
                bail!("linear commentCreate returned success=false for issue {issue_id}");
            }

            // Rate limit buffer between chunks
            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let issue_id = extract_issue_id(msg)?;

        let text = truncate_text(&msg.text, LINEAR_MAX_MESSAGE_LEN);

        let query = "mutation($issueId: String!, $body: String!) { \
            commentCreate(input: { issueId: $issueId, body: $body }) { \
                success comment { id } \
            } \
        }";

        let variables = serde_json::json!({
            "issueId": issue_id,
            "body": text,
        });

        let resp = self
            .graphql(query, variables)
            .await
            .with_context(|| format!("failed to create linear comment on issue {issue_id}"))?;

        let success = resp
            .pointer("/data/commentCreate/success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !success {
            bail!("linear commentCreate returned success=false for issue {issue_id}");
        }

        let comment_id = resp
            .pointer("/data/commentCreate/comment/id")
            .and_then(|v| v.as_str())
            .context("missing comment id in linear commentCreate response")?
            .to_string();

        Ok(comment_id)
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let text = truncate_text(&msg.text, LINEAR_MAX_MESSAGE_LEN);

        let query = "mutation($commentId: String!, $body: String!) { \
            commentUpdate(id: $commentId, input: { body: $body }) { \
                success \
            } \
        }";

        let variables = serde_json::json!({
            "commentId": platform_msg_id,
            "body": text,
        });

        let resp = self
            .graphql(query, variables)
            .await
            .with_context(|| format!("failed to edit linear comment {platform_msg_id}"))?;

        let success = resp
            .pointer("/data/commentUpdate/success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !success {
            bail!("linear commentUpdate returned success=false for comment {platform_msg_id}");
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
    api_key: String,
    client: reqwest::Client,
    bot_user_id: String,
    user_cache: Arc<RwLock<HashMap<String, String>>>,
    issue_cache: Arc<RwLock<HashMap<String, (String, String)>>>,
}

impl WebhookState {
    /// Resolve a Linear user ID to a display name, using the cache first.
    async fn resolve_user_name(&self, user_id: &str) -> Result<String> {
        // Check cache
        {
            let cache = self.user_cache.read().await;
            if let Some(name) = cache.get(user_id) {
                return Ok(name.clone());
            }
        }

        // Query the API
        let query = "query($userId: String!) { user(id: $userId) { name } }";
        let variables = serde_json::json!({ "userId": user_id });
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(LINEAR_API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to query linear user {user_id}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("linear user query failed: {status} {text}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse linear user query response")?;

        let name = json
            .pointer("/data/user/name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        // Store in cache
        {
            let mut cache = self.user_cache.write().await;
            cache.insert(user_id.to_string(), name.clone());
        }

        Ok(name)
    }

    /// Resolve a Linear issue ID to (identifier, team_key), using the cache first.
    async fn resolve_issue(&self, issue_id: &str) -> Result<(String, String)> {
        // Check cache
        {
            let cache = self.issue_cache.read().await;
            if let Some(entry) = cache.get(issue_id) {
                return Ok(entry.clone());
            }
        }

        // Query the API
        let query = "query($issueId: String!) { issue(id: $issueId) { identifier team { key } } }";
        let variables = serde_json::json!({ "issueId": issue_id });
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(LINEAR_API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to query linear issue {issue_id}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("linear issue query failed: {status} {text}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse linear issue query response")?;

        let identifier = json
            .pointer("/data/issue/identifier")
            .and_then(|v| v.as_str())
            .context("missing identifier in linear issue query response")?
            .to_string();

        let team_key = json
            .pointer("/data/issue/team/key")
            .and_then(|v| v.as_str())
            .context("missing team.key in linear issue query response")?
            .to_string();

        // Store in cache
        {
            let mut cache = self.issue_cache.write().await;
            cache.insert(issue_id.to_string(), (identifier.clone(), team_key.clone()));
        }

        Ok((identifier, team_key))
    }
}

/// Linear provider: validates API key, runs webhook server for inbound events.
pub struct LinearProvider {
    api_key: String,
    client: reqwest::Client,
    webhook_port: u16,
    bot_user_id: String,
    user_cache: Arc<RwLock<HashMap<String, String>>>,
    issue_cache: Arc<RwLock<HashMap<String, (String, String)>>>,
}

impl LinearProvider {
    /// Create a new Linear provider. Validates the API key via `{ viewer { id name } }`.
    pub async fn new(api_key: &str, webhook_port: u16) -> Result<Self> {
        let client = reqwest::Client::new();

        let body = serde_json::json!({
            "query": "{ viewer { id name } }",
        });

        let resp = client
            .post(LINEAR_API_URL)
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to call linear viewer query for api key validation")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let resp_body = resp.text().await.unwrap_or_default();
            bail!("linear api key validation failed: {status} {resp_body}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse linear viewer query response")?;

        if let Some(errors) = json.get("errors") {
            bail!("linear viewer query returned errors: {errors}");
        }

        let bot_user_id = json
            .pointer("/data/viewer/id")
            .and_then(|v| v.as_str())
            .context("linear viewer query missing id field")?
            .to_string();

        let bot_name = json
            .pointer("/data/viewer/name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        info!(bot_user_id, bot_name, "linear bot connected");

        Ok(Self {
            api_key: api_key.to_string(),
            client,
            webhook_port,
            bot_user_id,
            user_cache: Arc::new(RwLock::new(HashMap::new())),
            issue_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Returns the authenticated user's Linear user ID.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> LinearOutbound {
        LinearOutbound {
            api_key: self.api_key.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the webhook HTTP server for inbound events. Runs forever.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let state = Arc::new(WebhookState {
            tx,
            api_key: self.api_key.clone(),
            client: self.client.clone(),
            bot_user_id: self.bot_user_id.clone(),
            user_cache: self.user_cache,
            issue_cache: self.issue_cache,
        });

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .route("/healthz", get(handle_healthz))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        info!(addr, "starting linear webhook server");

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, addr, "failed to bind linear webhook server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "linear webhook server error");
        }
    }
}

/// Health check endpoint.
async fn handle_healthz() -> StatusCode {
    StatusCode::OK
}

/// Webhook handler: receives Linear webhook payloads and dispatches events.
async fn handle_webhook(State(state): State<Arc<WebhookState>>, body: String) -> StatusCode {
    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse linear webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    let event_type = payload["type"].as_str().unwrap_or("");
    let action = payload["action"].as_str().unwrap_or("");

    debug!(event_type, action, "received linear webhook event");

    match (event_type, action) {
        ("Comment", "create") => {
            handle_comment_create(&state, &payload).await;
        }
        _ => {
            debug!(
                event_type,
                action, "ignoring unhandled linear webhook event"
            );
        }
    }

    StatusCode::OK
}

/// Handle Linear Comment create webhook events.
async fn handle_comment_create(state: &WebhookState, payload: &serde_json::Value) {
    let data = &payload["data"];

    let user_id = match data["userId"].as_str() {
        Some(id) => id,
        None => {
            warn!("linear comment webhook missing data.userId");
            return;
        }
    };

    // Skip comments from the bot itself
    if user_id == state.bot_user_id {
        debug!("skipping own linear comment");
        return;
    }

    let issue_id = match data["issueId"].as_str() {
        Some(id) => id,
        None => {
            warn!("linear comment webhook missing data.issueId");
            return;
        }
    };

    let comment_body = data["body"].as_str().unwrap_or_default().to_string();
    if comment_body.is_empty() {
        return;
    }

    let comment_id = data["id"].as_str().unwrap_or_default().to_string();
    let created_at = data["createdAt"].as_str().unwrap_or_default();
    let timestamp = parse_linear_timestamp(created_at);

    // Resolve user name
    let sender_name = match state.resolve_user_name(user_id).await {
        Ok(name) => name,
        Err(e) => {
            warn!(error = %e, user_id, "failed to resolve linear user name, using fallback");
            user_id.to_string()
        }
    };

    // Resolve issue identifier and team key
    let (issue_identifier, team_key) = match state.resolve_issue(issue_id).await {
        Ok(result) => result,
        Err(e) => {
            warn!(error = %e, issue_id, "failed to resolve linear issue, skipping comment");
            return;
        }
    };

    let chat_id = team_key.clone();
    let thread_id = issue_identifier.clone();

    let raw = InboundRaw {
        chat_id,
        sender_name,
        text: comment_body,
        timestamp,
        thread_id: Some(thread_id),
        meta: serde_json::json!({
            "issue_id": issue_id,
            "comment_id": comment_id,
            "team_key": team_key,
            "issue_identifier": issue_identifier,
        }),
    };

    if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
        error!(error = %e, "failed to send linear inbound message to processor");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract `issue_id` from the outbound message meta.
fn extract_issue_id(msg: &OutboundMessage) -> Result<String> {
    msg.meta
        .get("issue_id")
        .and_then(|v| v.as_str())
        .context("outbound message missing meta.issue_id")
        .map(|s| s.to_string())
}

/// Truncate text to fit within the platform's max message length.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}

/// Parse a Linear ISO 8601 timestamp (e.g. "2024-01-15T12:00:00.000Z") to Unix seconds.
fn parse_linear_timestamp(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0)
}
