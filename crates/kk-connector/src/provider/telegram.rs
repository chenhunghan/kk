use anyhow::{Context, Result};
use async_trait::async_trait;
use teloxide::prelude::*;
use teloxide::types::{ChatMemberStatus, Me, MessageId, ReplyParameters, ThreadId};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use kk_core::text::split_text;
use kk_core::types::OutboundMessage;

use super::{ChatProvider, ConnectorEvent, InboundRaw};

const TELEGRAM_MAX_MESSAGE_LEN: usize = 4096;

pub struct TelegramProvider {
    bot: Bot,
    me: Me,
}

impl TelegramProvider {
    /// Create a new Telegram provider. Validates token via `get_me()`.
    pub async fn new(token: &str) -> Result<Self> {
        let bot = Bot::new(token);
        let me = bot
            .get_me()
            .await
            .context("failed to validate Telegram bot token (get_me)")?;

        info!(username = me.username(), "telegram bot connected");

        Ok(Self { bot, me })
    }

    /// Returns the bot's username (e.g. `"my_bot"`).
    pub fn bot_username(&self) -> &str {
        self.me.username()
    }

    /// Create a sender handle for the outbound poller.
    pub fn sender(&self) -> TelegramOutbound {
        TelegramOutbound {
            bot: self.bot.clone(),
        }
    }

    /// Start the inbound message dispatcher.
    /// Filters messages and `my_chat_member` updates, sends `ConnectorEvent` through the channel.
    pub async fn run_inbound(self, tx: mpsc::Sender<ConnectorEvent>) {
        let bot_id = self.me.id;
        let message_handler = Update::filter_message().endpoint(
            move |msg: Message, tx: mpsc::Sender<ConnectorEvent>| async move {
                // Skip messages from bots
                if let Some(ref from) = msg.from {
                    if from.is_bot {
                        debug!(chat_id = %msg.chat.id, "skipping bot message");
                        return respond(());
                    }
                } else {
                    return respond(());
                }

                // Skip our own bot messages
                if msg.from.as_ref().is_some_and(|f| f.id == bot_id) {
                    return respond(());
                }

                let chat_id_str = msg.chat.id.0.to_string();

                // Extract text (message text or caption for media)
                let text = msg
                    .text()
                    .or_else(|| msg.caption())
                    .unwrap_or_default()
                    .to_string();

                if text.is_empty() {
                    debug!(chat_id = %chat_id_str, "skipping non-text message");
                    return respond(());
                }

                // Extract sender name
                let from = msg.from.as_ref().unwrap();
                let sender_name = match &from.last_name {
                    Some(last) => format!("{} {last}", from.first_name),
                    None => from.first_name.clone(),
                };

                let timestamp = msg.date.timestamp() as u64;

                // Extract thread_id for Forum Topics / reply threads
                let thread_id: Option<String> = msg.thread_id.map(|tid| tid.0.to_string());

                let raw = InboundRaw {
                    chat_id: chat_id_str.clone(),
                    sender_name,
                    text,
                    timestamp,
                    thread_id,
                    meta: serde_json::json!({
                        "chat_id": chat_id_str,
                        "message_id": msg.id.0,
                    }),
                };

                if let Err(e) = tx.send(ConnectorEvent::Message(raw)).await {
                    error!(error = %e, "failed to send inbound message to processor");
                }

                respond(())
            },
        );

        let chat_member_handler = Update::filter_my_chat_member().endpoint(
            move |update: ChatMemberUpdated, tx: mpsc::Sender<ConnectorEvent>| async move {
                let old_present = matches!(
                    update.old_chat_member.status(),
                    ChatMemberStatus::Owner
                        | ChatMemberStatus::Administrator
                        | ChatMemberStatus::Member
                );
                let new_present = matches!(
                    update.new_chat_member.status(),
                    ChatMemberStatus::Owner
                        | ChatMemberStatus::Administrator
                        | ChatMemberStatus::Member
                );

                // Only fire when transitioning from not-present to present
                if !old_present && new_present {
                    let chat_id = update.chat.id.0.to_string();
                    let chat_title = update.chat.title().map(|s| s.to_string());
                    info!(chat_id, chat_title, "bot added to chat");

                    if let Err(e) = tx
                        .send(ConnectorEvent::NewChat {
                            chat_id,
                            chat_title,
                        })
                        .await
                    {
                        error!(error = %e, "failed to send new chat event");
                    }
                }

                respond(())
            },
        );

        let handler = dptree::entry()
            .branch(message_handler)
            .branch(chat_member_handler);

        Dispatcher::builder(self.bot, handler)
            .dependencies(dptree::deps![tx])
            .default_handler(|_upd| async {})
            .build()
            .dispatch()
            .await;
    }
}

/// Outbound sender for Telegram — implements `ChatProvider`.
pub struct TelegramOutbound {
    bot: Bot,
}

impl TelegramOutbound {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }
}

#[async_trait]
impl ChatProvider for TelegramOutbound {
    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let (chat_id, chat_id_str) = extract_chat_id(msg)?;
        let tg_thread_id = extract_thread_id(msg);

        let chunks = split_text(&msg.text, TELEGRAM_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut req = self.bot.send_message(chat_id, chunk);

            if let Some(tid) = tg_thread_id {
                req = req.message_thread_id(tid);
            }

            if i == 0
                && let Some(reply_id) = msg.meta.get("reply_to_message_id")
                && let Some(id) = reply_id.as_i64()
            {
                req = req.reply_parameters(ReplyParameters::new(MessageId(id as i32)));
            }

            req.await.with_context(|| {
                format!("failed to send telegram message to chat {chat_id_str}")
            })?;

            if chunks.len() > 1 && i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        Ok(())
    }

    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String> {
        let (chat_id, chat_id_str) = extract_chat_id(msg)?;
        let tg_thread_id = extract_thread_id(msg);

        let text = truncate_text(&msg.text, TELEGRAM_MAX_MESSAGE_LEN);
        let mut req = self.bot.send_message(chat_id, &text);

        if let Some(tid) = tg_thread_id {
            req = req.message_thread_id(tid);
        }

        if let Some(reply_id) = msg.meta.get("reply_to_message_id")
            && let Some(id) = reply_id.as_i64()
        {
            req = req.reply_parameters(ReplyParameters::new(MessageId(id as i32)));
        }

        let sent = req
            .await
            .with_context(|| format!("failed to send telegram message to chat {chat_id_str}"))?;

        Ok(sent.id.0.to_string())
    }

    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let (chat_id, chat_id_str) = extract_chat_id(msg)?;
        let msg_id = MessageId(
            platform_msg_id
                .parse::<i32>()
                .context("invalid telegram message_id")?,
        );

        let text = truncate_text(&msg.text, TELEGRAM_MAX_MESSAGE_LEN);

        match self.bot.edit_message_text(chat_id, msg_id, &text).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                // "message is not modified" is expected when text hasn't changed
                if err_str.contains("message is not modified") {
                    Ok(())
                } else {
                    Err(e).with_context(|| {
                        format!(
                            "failed to edit telegram message {platform_msg_id} in chat {chat_id_str}"
                        )
                    })
                }
            }
        }
    }
}

fn extract_chat_id(msg: &OutboundMessage) -> Result<(ChatId, String)> {
    let chat_id_str = msg
        .meta
        .get("chat_id")
        .and_then(|v| v.as_str())
        .context("outbound message missing meta.chat_id")?
        .to_string();
    let chat_id = ChatId(
        chat_id_str
            .parse::<i64>()
            .context("invalid chat_id in meta")?,
    );
    Ok((chat_id, chat_id_str))
}

fn extract_thread_id(msg: &OutboundMessage) -> Option<ThreadId> {
    msg.thread_id
        .as_deref()
        .and_then(|tid| tid.parse::<i32>().ok())
        .map(|id| ThreadId(MessageId(id)))
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        text[..max_len].to_string()
    }
}
