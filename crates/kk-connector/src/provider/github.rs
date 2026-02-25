use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const GITHUB_MAX_MESSAGE_LEN: usize = 65536;
const GITHUB_API_BASE: &str = "https://api.github.com";

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Handle for sending outbound messages via GitHub REST API.
#[derive(Clone)]
pub struct GithubOutbound {
    token: String,
    client: reqwest::Client,
}

impl GithubOutbound {
    pub fn new(token: String, client: reqwest::Client) -> Self {
        Self { token, client }
    }
}

#[async_trait]
impl ChatProvider for GithubOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let (owner, repo) = extract_owner_repo(msg)?;
        let issue_number = extract_issue_number(msg)?;

        let chunks = split_text(&msg.text, GITHUB_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let url =
                format!("{GITHUB_API_BASE}/repos/{owner}/{repo}/issues/{issue_number}/comments");

            let body = serde_json::json!({ "body": chunk });

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "kk-connector")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&body)
                .send()
                .await
                .with_context(|| {
                    format!("failed to create github comment on {owner}/{repo}#{issue_number}")
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                bail!(
                    "github create comment failed for {owner}/{repo}#{issue_number}: {status} {text}"
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
        let (owner, repo) = extract_owner_repo(msg)?;
        let issue_number = extract_issue_number(msg)?;

        let text = truncate_text(&msg.text, GITHUB_MAX_MESSAGE_LEN);
        let url = format!("{GITHUB_API_BASE}/repos/{owner}/{repo}/issues/{issue_number}/comments");
        let body = serde_json::json!({ "body": text });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "kk-connector")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to create github comment on {owner}/{repo}#{issue_number}")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "github create comment failed for {owner}/{repo}#{issue_number}: {status} {text}"
            );
        }

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse github create comment response")?;

        let comment_id = resp_json["id"]
            .as_u64()
            .context("missing id in github create comment response")?;

        Ok(comment_id.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let (owner, repo) = extract_owner_repo(msg)?;

        let text = truncate_text(&msg.text, GITHUB_MAX_MESSAGE_LEN);
        let url =
            format!("{GITHUB_API_BASE}/repos/{owner}/{repo}/issues/comments/{platform_msg_id}");
        let body = serde_json::json!({ "body": text });

        let resp = self
            .client
            .patch(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "kk-connector")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("failed to edit github comment {platform_msg_id} on {owner}/{repo}")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "github edit comment failed for {owner}/{repo} comment {platform_msg_id}: {status} {text}"
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
    bot_login: String,
}

/// GitHub provider: validates token, runs webhook server for inbound events.
pub struct GithubProvider {
    token: String,
    client: reqwest::Client,
    webhook_port: u16,
    #[allow(dead_code)]
    webhook_secret: Option<String>,
    bot_login: String,
}

impl GithubProvider {
    /// Create a new GitHub provider. Validates token via `GET /user`.
    pub async fn new(
        token: &str,
        webhook_port: u16,
        webhook_secret: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("{GITHUB_API_BASE}/user"))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "kk-connector")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("failed to call GET /user for token validation")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("github token validation failed: {status} {body}");
        }

        let user: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse GET /user response")?;

        let bot_login = user["login"]
            .as_str()
            .context("GET /user missing login field")?
            .to_string();

        info!(bot_login, "github bot connected");

        Ok(Self {
            token: token.to_string(),
            client,
            webhook_port,
            webhook_secret,
            bot_login,
        })
    }

    /// Returns the authenticated user's login (e.g. `"my-bot"`).
    pub fn bot_login(&self) -> &str {
        &self.bot_login
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> GithubOutbound {
        GithubOutbound {
            token: self.token.clone(),
            client: self.client.clone(),
        }
    }

    /// Start the webhook HTTP server for inbound events. Runs forever.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let state = Arc::new(WebhookState {
            tx,
            bot_login: self.bot_login.clone(),
        });

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .route("/healthz", get(handle_healthz))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        info!(addr, "starting github webhook server");

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, addr, "failed to bind github webhook server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "github webhook server error");
        }
    }
}

/// Health check endpoint.
async fn handle_healthz() -> StatusCode {
    StatusCode::OK
}

/// Webhook handler: receives GitHub webhook payloads and dispatches events.
async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    // TODO: Verify X-Hub-Signature-256 using HMAC-SHA256 with GITHUB_WEBHOOK_SECRET
    // For now, accept all payloads.

    let event_type = match headers.get("x-github-event").and_then(|v| v.to_str().ok()) {
        Some(et) => et.to_string(),
        None => {
            warn!("webhook request missing X-GitHub-Event header");
            return StatusCode::BAD_REQUEST;
        }
    };

    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    debug!(event_type, "received github webhook event");

    match event_type.as_str() {
        "issue_comment" => {
            handle_issue_comment(&state, &payload).await;
        }
        "pull_request_review_comment" => {
            handle_pr_review_comment(&state, &payload).await;
        }
        _ => {
            debug!(event_type, "ignoring unhandled github event type");
        }
    }

    StatusCode::OK
}

/// Handle `issue_comment` webhook events.
async fn handle_issue_comment(state: &WebhookState, payload: &serde_json::Value) {
    let action = payload["action"].as_str().unwrap_or("");
    if action != "created" {
        debug!(action, "skipping issue_comment with non-created action");
        return;
    }

    let comment = &payload["comment"];
    let user = &comment["user"];

    // Skip bot comments
    if user["type"].as_str().unwrap_or("") == "Bot" {
        debug!("skipping bot comment");
        return;
    }

    // Skip our own comments
    let login = user["login"].as_str().unwrap_or("");
    if login == state.bot_login {
        debug!("skipping own comment");
        return;
    }

    let body_text = comment["body"].as_str().unwrap_or_default().to_string();
    if body_text.is_empty() {
        return;
    }

    let repo_full_name = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or_default();
    let (owner, repo) = match repo_full_name.split_once('/') {
        Some((o, r)) => (o.to_string(), r.to_string()),
        None => {
            warn!(repo_full_name, "invalid repository full_name in webhook");
            return;
        }
    };

    let issue_number = payload["issue"]["number"].as_u64().unwrap_or(0);
    let comment_id = comment["id"].as_u64().unwrap_or(0);
    let sender_name = login.to_string();
    let timestamp = parse_github_timestamp(comment["created_at"].as_str().unwrap_or_default());

    let chat_id = format!("{owner}/{repo}");
    let thread_id = issue_number.to_string();

    let raw = InboundRaw {
        chat_id,
        sender_name,
        text: body_text,
        timestamp,
        thread_id: Some(thread_id),
        meta: serde_json::json!({
            "owner": owner,
            "repo": repo,
            "issue_number": issue_number,
            "comment_id": comment_id,
        }),
    };

    if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
        error!(error = %e, "failed to send github inbound message to processor");
    }
}

/// Handle `pull_request_review_comment` webhook events.
async fn handle_pr_review_comment(state: &WebhookState, payload: &serde_json::Value) {
    let action = payload["action"].as_str().unwrap_or("");
    if action != "created" {
        debug!(
            action,
            "skipping pull_request_review_comment with non-created action"
        );
        return;
    }

    let comment = &payload["comment"];
    let user = &comment["user"];

    // Skip bot comments
    if user["type"].as_str().unwrap_or("") == "Bot" {
        debug!("skipping bot PR review comment");
        return;
    }

    // Skip our own comments
    let login = user["login"].as_str().unwrap_or("");
    if login == state.bot_login {
        debug!("skipping own PR review comment");
        return;
    }

    let body_text = comment["body"].as_str().unwrap_or_default().to_string();
    if body_text.is_empty() {
        return;
    }

    let repo_full_name = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or_default();
    let (owner, repo) = match repo_full_name.split_once('/') {
        Some((o, r)) => (o.to_string(), r.to_string()),
        None => {
            warn!(
                repo_full_name,
                "invalid repository full_name in PR review comment webhook"
            );
            return;
        }
    };

    let pr_number = payload["pull_request"]["number"].as_u64().unwrap_or(0);
    let comment_id = comment["id"].as_u64().unwrap_or(0);
    let sender_name = login.to_string();
    let timestamp = parse_github_timestamp(comment["created_at"].as_str().unwrap_or_default());

    let chat_id = format!("{owner}/{repo}");
    let thread_id = pr_number.to_string();

    let raw = InboundRaw {
        chat_id,
        sender_name,
        text: body_text,
        timestamp,
        thread_id: Some(thread_id),
        meta: serde_json::json!({
            "owner": owner,
            "repo": repo,
            "issue_number": pr_number,
            "comment_id": comment_id,
        }),
    };

    if let Err(e) = state.tx.send(ConnectorEvent::Message(raw)).await {
        error!(error = %e, "failed to send github PR review comment to processor");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract `owner` and `repo` from the outbound message meta.
fn extract_owner_repo(msg: &OutboundMessage) -> Result<(String, String)> {
    let owner = msg
        .meta
        .get("owner")
        .and_then(|v| v.as_str())
        .context("outbound message missing meta.owner")?
        .to_string();
    let repo = msg
        .meta
        .get("repo")
        .and_then(|v| v.as_str())
        .context("outbound message missing meta.repo")?
        .to_string();
    Ok((owner, repo))
}

/// Extract `issue_number` from the outbound message meta.
fn extract_issue_number(msg: &OutboundMessage) -> Result<u64> {
    msg.meta
        .get("issue_number")
        .and_then(|v| v.as_u64())
        .context("outbound message missing meta.issue_number")
}

/// Truncate text to fit within the platform's max message length.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}

/// Parse a GitHub ISO 8601 timestamp (e.g. "2024-01-15T12:34:56Z") to Unix seconds.
fn parse_github_timestamp(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0)
}
