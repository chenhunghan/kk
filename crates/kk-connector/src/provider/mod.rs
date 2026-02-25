pub mod slack;
pub mod telegram;

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
}
