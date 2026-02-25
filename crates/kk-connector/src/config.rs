use anyhow::{Result, bail};
use std::env;

use kk_core::types::ChannelType;

#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    pub channel_type: String,
    pub channel_name: String,
    pub inbound_dir: String,
    pub outbox_dir: String,
    pub stream_dir: String,
    pub groups_d_file: String,
    pub telegram_bot_token: Option<String>,
    pub slack_bot_token: Option<String>,
    pub slack_app_token: Option<String>,
    pub discord_bot_token: Option<String>,
    pub github_token: Option<String>,
    pub github_webhook_secret: Option<String>,
    pub github_webhook_port: u16,
    pub whatsapp_token: Option<String>,
    pub whatsapp_phone_number_id: Option<String>,
    pub whatsapp_webhook_verify_token: Option<String>,
    pub whatsapp_webhook_port: u16,
    pub outbound_poll_interval_ms: u64,
}

impl ConnectorConfig {
    pub fn from_env() -> Result<Self> {
        let channel_type = env::var("CHANNEL_TYPE")
            .map_err(|_| anyhow::anyhow!("CHANNEL_TYPE env var required"))?;
        let channel_name = env::var("CHANNEL_NAME")
            .map_err(|_| anyhow::anyhow!("CHANNEL_NAME env var required"))?;

        if channel_type.is_empty() {
            bail!("CHANNEL_TYPE cannot be empty");
        }
        if channel_name.is_empty() {
            bail!("CHANNEL_NAME cannot be empty");
        }

        Ok(Self {
            channel_type,
            channel_name: channel_name.clone(),
            inbound_dir: env::var("INBOUND_DIR").unwrap_or_else(|_| "/data/inbound".into()),
            outbox_dir: env::var("OUTBOX_DIR")
                .unwrap_or_else(|_| format!("/data/outbox/{channel_name}")),
            stream_dir: env::var("STREAM_DIR")
                .unwrap_or_else(|_| format!("/data/stream/{channel_name}")),
            groups_d_file: env::var("GROUPS_D_FILE")
                .unwrap_or_else(|_| format!("/data/state/groups.d/{channel_name}.json")),
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
            slack_bot_token: env::var("SLACK_BOT_TOKEN").ok(),
            slack_app_token: env::var("SLACK_APP_TOKEN").ok(),
            discord_bot_token: env::var("DISCORD_BOT_TOKEN").ok(),
            github_token: env::var("GITHUB_TOKEN").ok(),
            github_webhook_secret: env::var("GITHUB_WEBHOOK_SECRET").ok(),
            github_webhook_port: env::var("GITHUB_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8084),
            whatsapp_token: env::var("WHATSAPP_TOKEN").ok(),
            whatsapp_phone_number_id: env::var("WHATSAPP_PHONE_NUMBER_ID").ok(),
            whatsapp_webhook_verify_token: env::var("WHATSAPP_WEBHOOK_VERIFY_TOKEN").ok(),
            whatsapp_webhook_port: env::var("WHATSAPP_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8085),
            outbound_poll_interval_ms: env::var("OUTBOUND_POLL_INTERVAL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_000),
        })
    }

    /// Map the channel_type string to the ChannelType enum.
    pub fn channel_type_enum(&self) -> ChannelType {
        match self.channel_type.as_str() {
            "telegram" => ChannelType::Telegram,
            "slack" => ChannelType::Slack,
            "discord" => ChannelType::Discord,
            "github" => ChannelType::Github,
            "whatsapp" => ChannelType::Whatsapp,
            other => panic!("unsupported channel type: {other}"),
        }
    }

    /// Return the auto-registration slug prefix for this channel type.
    pub fn slug_prefix(&self) -> &str {
        match self.channel_type.as_str() {
            "telegram" => "tg",
            "slack" => "slack",
            "discord" => "discord",
            "github" => "gh",
            "whatsapp" => "wa",
            _ => "ch",
        }
    }
}
