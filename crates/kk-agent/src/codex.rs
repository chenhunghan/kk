use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::Command;
use kk_core::types::ResultLine;

use crate::agent::{CodeAgent, AgentResult};

pub struct Codex;

impl CodeAgent for Codex {
    fn spawn(
        &self,
        bin: &str,
        prompt: &str,
        _max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult> {
        let stdout_file = File::create(response_path)
            .with_context(|| format!("create {}", response_path.display()))?;
        let stderr_file =
            File::create(log_path).with_context(|| format!("create {}", log_path.display()))?;

        let status = Command::new(bin)
            .arg("exec")
            .arg(prompt)
            .arg("--json")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("--skip-git-repo-check")
            .current_dir(session_dir)
            .stdout(stdout_file)
            .stderr(stderr_file)
            .status()
            .with_context(|| format!("spawn {bin} exec"))?;

        Ok(AgentResult {
            exit_code: status.code().unwrap_or(1),
        })
    }

    fn spawn_with_session(
        &self,
        bin: &str,
        prompt: &str,
        session_id: &str,
        _max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult> {
        let stdout_file = File::create(response_path)
            .with_context(|| format!("create {}", response_path.display()))?;
        let stderr_file =
            File::create(log_path).with_context(|| format!("create {}", log_path.display()))?;

        let status = Command::new(bin)
            .arg("exec")
            .arg("resume")
            .arg(session_id)
            .arg(prompt)
            .arg("--json")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("--skip-git-repo-check")
            .current_dir(session_dir)
            .stdout(stdout_file)
            .stderr(stderr_file)
            .status()
            .with_context(|| format!("spawn {bin} exec resume {session_id}"))?;

        Ok(AgentResult {
            exit_code: status.code().unwrap_or(1),
        })
    }

    fn resume(
        &self,
        bin: &str,
        prompt: &str,
        session_id: Option<&str>,
        _max_turns: u32,
        session_dir: &Path,
        response_path: &Path,
        log_path: &Path,
    ) -> Result<AgentResult> {
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(response_path)
            .with_context(|| format!("open {} for append", response_path.display()))?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("open {} for append", log_path.display()))?;

        let mut cmd = Command::new(bin);
        cmd.arg("exec").arg("resume");
        
        if let Some(sid) = session_id {
            cmd.arg(sid);
        } else {
            cmd.arg("--last");
        }

        let status = cmd
            .arg(prompt)
            .arg("--json")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("--skip-git-repo-check")
            .current_dir(session_dir)
            .stdout(stdout_file)
            .stderr(stderr_file)
            .status()
            .with_context(|| format!("spawn {bin} exec resume"))?;

        Ok(AgentResult {
            exit_code: status.code().unwrap_or(1),
        })
    }

    fn extract_session_id(&self, response_path: &Path) -> Option<String> {
        let content = std::fs::read_to_string(response_path).ok()?;
        for line in content.lines().rev() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(rl) = serde_json::from_str::<ResultLine>(line)
                && let Some(sid) = rl.session_id
            {
                return Some(sid);
            }
        }
        None
    }

    fn detect_overflow(&self, response_path: &Path, log_path: &Path) -> bool {
        let overflow_patterns = [
            "context_length_exceeded",
            "context window",
            "maximum context length",
            "token limit",
            "too many tokens",
        ];
        for path in [response_path, log_path] {
            if let Ok(content) = std::fs::read_to_string(path) {
                let lower = content.to_lowercase();
                for pattern in &overflow_patterns {
                    if lower.contains(pattern) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn skill_dir_name(&self) -> &str {
        ".codex"
    }
}
