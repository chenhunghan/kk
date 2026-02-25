use std::collections::HashMap;

use serde::Deserialize;

/// Top-level kk.yaml config for lean mode.
#[derive(Debug, Deserialize)]
pub struct KkConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,

    #[serde(default = "default_agent_bin")]
    pub agent_bin: String,

    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,

    #[serde(default = "default_max_turns")]
    pub max_turns: u64,

    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    pub name: String,

    #[serde(rename = "type")]
    pub channel_type: String,

    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_data_dir() -> String {
    "./data".into()
}
fn default_agent_bin() -> String {
    "kk-agent".into()
}
fn default_idle_timeout() -> u64 {
    120
}
fn default_max_turns() -> u64 {
    25
}

impl Default for KkConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            agent_bin: default_agent_bin(),
            idle_timeout: default_idle_timeout(),
            max_turns: default_max_turns(),
            channels: Vec::new(),
        }
    }
}

impl KkConfig {
    /// Load config from kk.yaml in CWD, or use defaults if not found.
    pub fn load() -> anyhow::Result<Self> {
        match std::fs::read_to_string("kk.yaml") {
            Ok(content) => Ok(serde_yaml::from_str(&content)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}
