use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::Command;

pub struct ClaudeResult {
    pub exit_code: i32,
}

/// Spawn `claude -p` for an initial prompt.
/// stdout is written (create) to response_path, stderr to log_path.
pub fn spawn_claude(
    bin: &str,
    prompt: &str,
    max_turns: u32,
    session_dir: &Path,
    response_path: &Path,
    log_path: &Path,
) -> Result<ClaudeResult> {
    let stdout_file = File::create(response_path)
        .with_context(|| format!("create {}", response_path.display()))?;
    let stderr_file =
        File::create(log_path).with_context(|| format!("create {}", log_path.display()))?;

    let status = Command::new(bin)
        .arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--max-turns")
        .arg(max_turns.to_string())
        .arg("--verbose")
        .current_dir(session_dir)
        .stdout(stdout_file)
        .stderr(stderr_file)
        .status()
        .with_context(|| format!("spawn {bin}"))?;

    Ok(ClaudeResult {
        exit_code: status.code().unwrap_or(1),
    })
}

/// Spawn `claude -p --resume <session_id>` to resume a previous cross-Job session.
/// stdout is written (create) to response_path, stderr to log_path.
pub fn spawn_claude_with_session(
    bin: &str,
    prompt: &str,
    claude_session_id: &str,
    max_turns: u32,
    session_dir: &Path,
    response_path: &Path,
    log_path: &Path,
) -> Result<ClaudeResult> {
    let stdout_file = File::create(response_path)
        .with_context(|| format!("create {}", response_path.display()))?;
    let stderr_file =
        File::create(log_path).with_context(|| format!("create {}", log_path.display()))?;

    let status = Command::new(bin)
        .arg("-p")
        .arg(prompt)
        .arg("--resume")
        .arg(claude_session_id)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--max-turns")
        .arg(max_turns.to_string())
        .arg("--verbose")
        .current_dir(session_dir)
        .stdout(stdout_file)
        .stderr(stderr_file)
        .status()
        .with_context(|| format!("spawn {bin} --resume {claude_session_id}"))?;

    Ok(ClaudeResult {
        exit_code: status.code().unwrap_or(1),
    })
}

/// Spawn `claude -p --resume` for a follow-up message.
/// stdout/stderr are opened in append mode.
pub fn spawn_claude_resume(
    bin: &str,
    prompt: &str,
    max_turns: u32,
    session_dir: &Path,
    response_path: &Path,
    log_path: &Path,
) -> Result<ClaudeResult> {
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

    let status = Command::new(bin)
        .arg("-p")
        .arg(prompt)
        .arg("--resume")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--max-turns")
        .arg(max_turns.to_string())
        .arg("--verbose")
        .current_dir(session_dir)
        .stdout(stdout_file)
        .stderr(stderr_file)
        .status()
        .with_context(|| format!("spawn {bin} --resume"))?;

    Ok(ClaudeResult {
        exit_code: status.code().unwrap_or(1),
    })
}
