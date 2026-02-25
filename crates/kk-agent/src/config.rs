use anyhow::{Context, Result};
use std::env;

/// Agent configuration parsed from environment variables set by the Gateway.
pub struct AgentConfig {
    pub session_id: String,
    pub group: String,
    pub data_dir: String,
    pub idle_timeout: u64,
    pub max_turns: u32,
    pub thread_id: Option<String>,
    pub claude_bin: String,
}

impl AgentConfig {
    pub fn from_env() -> Result<Self> {
        let session_id = env::var("SESSION_ID").context("SESSION_ID env var required")?;
        let group = env::var("GROUP").context("GROUP env var required")?;
        let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
        let idle_timeout = env::var("IDLE_TIMEOUT")
            .unwrap_or_else(|_| "120".to_string())
            .parse::<u64>()
            .context("IDLE_TIMEOUT must be a number")?;
        let max_turns = env::var("MAX_TURNS")
            .unwrap_or_else(|_| "25".to_string())
            .parse::<u32>()
            .context("MAX_TURNS must be a number")?;
        let thread_id = env::var("THREAD_ID").ok().filter(|s| !s.is_empty());
        let claude_bin = env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

        Ok(Self {
            session_id,
            group,
            data_dir,
            idle_timeout,
            max_turns,
            thread_id,
            claude_bin,
        })
    }
}
