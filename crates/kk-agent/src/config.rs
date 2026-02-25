use anyhow::{Context, Result};
use std::env;
use crate::agent::CodeAgent;

/// Supported agent types.
pub enum AgentType {
    Claude,
    Gemini,
}

impl AgentType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }

    pub fn get_agent(&self) -> Box<dyn CodeAgent> {
        match self {
            Self::Claude => Box::new(crate::claude::Claude),
            Self::Gemini => Box::new(crate::gemini::Gemini),
        }
    }
}

/// Agent configuration parsed from environment variables set by the Gateway.
pub struct AgentConfig {
    pub session_id: String,
    pub group: String,
    pub data_dir: String,
    pub idle_timeout: u64,
    pub max_turns: u32,
    pub thread_id: Option<String>,
    pub agent_type: AgentType,
    pub agent_bin: String,
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
        
        let agent_type_str = env::var("AGENT_TYPE").unwrap_or_else(|_| "claude".to_string());
        let agent_type = AgentType::from_str(&agent_type_str)
            .with_context(|| format!("invalid AGENT_TYPE: {agent_type_str}"))?;

        let agent_bin = env::var("AGENT_BIN").or_else(|_| env::var("CLAUDE_BIN")).unwrap_or_else(|_| {
            match agent_type {
                AgentType::Claude => "claude".to_string(),
                AgentType::Gemini => "gemini".to_string(),
            }
        });

        Ok(Self {
            session_id,
            group,
            data_dir,
            idle_timeout,
            max_turns,
            thread_id,
            agent_type,
            agent_bin,
        })
    }
}
