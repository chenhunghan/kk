pub mod slack;
pub mod telegram;

use anyhow::Result;

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

/// Abstraction over provider-specific send implementations.
pub enum ProviderSender {
    Telegram(teloxide::Bot),
    Slack(slack::SlackSender),
}

impl ProviderSender {
    pub async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        match self {
            Self::Telegram(bot) => telegram::TelegramProvider::send(bot, msg).await,
            Self::Slack(sender) => slack::SlackProvider::send(sender, msg).await,
        }
    }
}
