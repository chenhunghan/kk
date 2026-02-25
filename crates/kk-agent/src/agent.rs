use anyhow::Result;
use std::path::Path;

pub struct AgentResult {
    pub exit_code: i32,
}

pub trait CodeAgent {
    /// Spawn the agent for an initial prompt.
    fn spawn(
        &self,
        bin: &str,
        prompt: &str,
        max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult>;

    /// Spawn the agent with a specific session ID to resume a previous cross-Job session.
    fn spawn_with_session(
        &self,
        bin: &str,
        prompt: &str,
        session_id: &str,
        max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult>;

    /// Spawn the agent to resume a session (for follow-ups).
    fn resume(
        &self,
        bin: &str,
        prompt: &str,
        session_id: Option<&str>,
        max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult>;

    /// Extract the agent's internal session ID from the response file.
    fn extract_session_id(&self, response_path: &Path) -> Option<String>;

    /// Detect if the agent hit a context window overflow.
    fn detect_overflow(&self, response_path: &Path, log_path: &Path) -> bool;

    /// The name of the directory where the agent expects its skills (e.g., ".claude").
    fn skill_dir_name(&self) -> &str;
}
