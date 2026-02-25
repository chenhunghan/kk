pub mod discord;
pub mod gchat;
pub mod github;
pub mod linear;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod whatsapp;

use anyhow::Result;
use async_trait::async_trait;

use kk_core::types::OutboundMessage;

/// Raw inbound data from a provider, before normalization.
#[derive(Debug)]
pub struct InboundRaw {
    pub chat_id: String,
    pub sender_name: String,
    pub text: String,
    pub timestamp: u64,
    pub thread_id: Option<String>,
    pub meta: serde_json::Value,
}

/// Events from a provider to the inbound processor.
#[derive(Debug)]
pub enum ConnectorEvent {
    /// A regular inbound message.
    Message(InboundRaw),
    /// Bot was added to a new chat (e.g. Telegram `my_chat_member` update).
    NewChat {
        chat_id: String,
        chat_title: Option<String>,
    },
}

/// Trait that every chat platform provider must implement for outbound messaging.
///
/// Implementing `send()` + `edit()` gives fallback (edit-in-place) streaming for free.
/// The outbound loop in `outbound.rs` handles the streaming coordination using these
/// three methods — no provider needs to know about streaming state files.
///
/// Providers that support native streaming (e.g. Slack `chat.startStream`/`appendStream`/
/// `stopStream`) can override the `supports_native_stream` and `stream_*` methods.
/// When `supports_native_stream()` returns true, `poll_stream()` uses those methods
/// instead of the `send_returning_id`/`edit` fallback.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Send a new message. May split long messages across multiple platform messages.
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;

    /// Send a single message and return the platform-specific message ID.
    /// Used for streaming: the first partial response creates the message.
    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String>;

    /// Edit a previously sent message by its platform-specific ID.
    /// Used for streaming: updates the same message with new partial content.
    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()>;

    /// Whether this provider supports editing sent messages.
    /// Default: true — most platforms support edit-in-place.
    /// WhatsApp returns false since it has no edit API.
    fn supports_edit(&self) -> bool {
        true
    }

    /// Whether this provider supports native streaming (e.g. Slack).
    /// Default: false — use edit-in-place fallback.
    fn supports_native_stream(&self) -> bool {
        false
    }

    /// Start a native streaming session. Returns platform message ID.
    async fn stream_start(&self, _msg: &OutboundMessage) -> Result<String> {
        anyhow::bail!("native streaming not supported")
    }

    /// Append text to an active native stream.
    async fn stream_append(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        anyhow::bail!("native streaming not supported")
    }

    /// Finalize an active native stream.
    async fn stream_stop(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        anyhow::bail!("native streaming not supported")
    }
}
