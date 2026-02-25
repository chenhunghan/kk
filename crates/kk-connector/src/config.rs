use anyhow::{Result, bail};
use std::env;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConnectorConfig {
    pub channel_type: String,
    pub channel_name: String,
    pub inbound_dir: String,
    pub outbox_dir: String,
    pub groups_file: String,
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
            groups_file: env::var("GROUPS_FILE")
                .unwrap_or_else(|_| "/data/state/groups.json".into()),
        })
    }
}
