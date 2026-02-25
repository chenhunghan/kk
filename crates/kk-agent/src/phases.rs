use anyhow::{Context, Result, bail};
use kk_core::nq;
use kk_core::paths::DataPaths;
use kk_core::types::{FollowUpMessage, RequestManifest, ResultLine, ResultStatus};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::claude;
use crate::config::AgentConfig;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Phase 0: Symlink skills into session directory
// ---------------------------------------------------------------------------

pub fn phase_0_skills(paths: &DataPaths, session_dir: &Path) -> Result<()> {
    tracing::info!("phase 0: injecting skills");

    let skills_dir = paths.skills_dir();
    if !skills_dir.exists() {
        tracing::info!("no skills directory found");
        return Ok(());
    }

    let entries = std::fs::read_dir(&skills_dir).context("read skills directory")?;

    let claude_skills_dir = session_dir.join(".claude").join("skills");
    let agents_skills_dir = session_dir.join(".agents").join("skills");
    std::fs::create_dir_all(&claude_skills_dir)?;
    std::fs::create_dir_all(&agents_skills_dir)?;

    let mut count = 0u32;

    for entry in entries {
        let entry = entry?;
        let skill_path = entry.path();

        if !skill_path.is_dir() {
            continue;
        }

        let skill_name = entry.file_name();
        let skill_name_str = skill_name.to_string_lossy();

        if !skill_path.join("SKILL.md").exists() {
            tracing::warn!(skill = %skill_name_str, "skipping skill: SKILL.md not found");
            continue;
        }

        // Symlink into .claude/skills/
        let claude_link = claude_skills_dir.join(&skill_name);
        let _ = std::fs::remove_file(&claude_link);
        std::os::unix::fs::symlink(&skill_path, &claude_link)
            .with_context(|| format!("symlink skill '{skill_name_str}'"))?;

        // Symlink into .agents/skills/ (chained from .claude/skills/)
        let agents_link = agents_skills_dir.join(&skill_name);
        let _ = std::fs::remove_file(&agents_link);
        std::os::unix::fs::symlink(&claude_link, &agents_link)
            .with_context(|| format!("agents symlink for skill '{skill_name_str}'"))?;

        count += 1;
    }

    tracing::info!(count, "injected skills into session");
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1: Read prompt, build context, spawn claude
// ---------------------------------------------------------------------------

pub fn phase_1_prompt(config: &AgentConfig, paths: &DataPaths, session_dir: &Path) -> Result<()> {
    tracing::info!("phase 1: processing initial prompt");

    // Read request manifest
    let manifest_path = paths.request_manifest(&config.session_id);
    let manifest_data = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: RequestManifest =
        serde_json::from_str(&manifest_data).context("parse request manifest")?;

    // Extract prompt from last message
    let prompt_text = manifest
        .messages
        .last()
        .map(|m| m.text.as_str())
        .unwrap_or("");
    if prompt_text.is_empty() {
        bail!("no prompt text in request manifest");
    }

    // Build context from memory files
    let soul_content = std::fs::read_to_string(paths.soul_md()).ok();
    let group_content = std::fs::read_to_string(paths.group_claude_md(&config.group)).ok();

    if soul_content.is_some() {
        tracing::info!("loaded SOUL.md");
    }
    if group_content.is_some() {
        tracing::info!(group = %config.group, "loaded group CLAUDE.md");
    }

    let full_prompt = build_prompt(
        soul_content.as_deref(),
        group_content.as_deref(),
        prompt_text,
    );

    // Write status=running
    let status_path = paths.result_status(&config.session_id);
    std::fs::write(&status_path, ResultStatus::Running.as_str()).context("write status=running")?;

    // Spawn claude
    let response_path = paths.result_response(&config.session_id);
    let log_path = paths.results_dir(&config.session_id).join("agent.log");

    // Check for previous Claude session to resume
    let claude_sid_path = paths.claude_session_id_file(&config.group, config.thread_id.as_deref());
    let prev_claude_session = std::fs::read_to_string(&claude_sid_path)
        .ok()
        .filter(|s| !s.trim().is_empty());

    let result = if let Some(ref claude_sid) = prev_claude_session {
        tracing::info!(claude_session_id = %claude_sid.trim(), "resuming previous cross-Job session");
        claude::spawn_claude_with_session(
            &config.claude_bin,
            &full_prompt,
            claude_sid.trim(),
            config.max_turns,
            session_dir,
            &response_path,
            &log_path,
        )?
    } else {
        tracing::info!(
            max_turns = config.max_turns,
            "running claude -p (fresh session)"
        );
        claude::spawn_claude(
            &config.claude_bin,
            &full_prompt,
            config.max_turns,
            session_dir,
            &response_path,
            &log_path,
        )?
    };

    if result.exit_code == 1 {
        tracing::error!(exit_code = 1, "claude exited with fatal error");
        std::fs::write(&status_path, ResultStatus::Error.as_str()).context("write status=error")?;
        bail!("claude exited with fatal error (exit code 1)");
    }

    if result.exit_code != 0 {
        tracing::warn!(
            exit_code = result.exit_code,
            "claude exited with non-fatal error"
        );
    }

    // Check for context overflow
    if detect_context_overflow(&response_path, &log_path) {
        tracing::warn!("context overflow detected after phase 1");
        std::fs::write(&status_path, ResultStatus::Overflow.as_str())
            .context("write status=overflow")?;
        return Ok(());
    }

    // Persist Claude's session_id for cross-Job resume
    if let Some(claude_sid) = extract_claude_session_id(&response_path) {
        if let Some(parent) = claude_sid_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&claude_sid_path, &claude_sid) {
            tracing::warn!(error = %e, "failed to persist claude session_id");
        } else {
            tracing::info!(claude_session_id = %claude_sid, "persisted claude session_id");
        }
    }

    tracing::info!("phase 1 complete");
    Ok(())
}

/// Extract Claude's internal session_id from response.jsonl.
/// Scans backwards for efficiency — the session_id typically appears in the last "result" line.
fn extract_claude_session_id(response_path: &Path) -> Option<String> {
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

fn build_prompt(soul: Option<&str>, group: Option<&str>, prompt: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(s) = soul
        && !s.is_empty()
    {
        parts.push(s);
    }
    if let Some(g) = group
        && !g.is_empty()
    {
        parts.push(g);
    }
    parts.push(prompt);
    parts.join("\n\n---\n\n")
}

// ---------------------------------------------------------------------------
// Phase 2: Poll for follow-ups
// ---------------------------------------------------------------------------

pub fn phase_2_followups(
    config: &AgentConfig,
    paths: &DataPaths,
    session_dir: &Path,
) -> Result<()> {
    let queue_dir = paths.group_queue_dir_threaded(&config.group, config.thread_id.as_deref());
    let response_path = paths.result_response(&config.session_id);
    let log_path = paths.results_dir(&config.session_id).join("agent.log");

    tracing::info!(
        idle_timeout_secs = config.idle_timeout,
        "phase 2: polling for follow-ups"
    );

    let mut last_activity = Instant::now();
    let mut followup_count = 0u32;
    let idle_timeout = Duration::from_secs(config.idle_timeout);

    loop {
        if last_activity.elapsed() >= idle_timeout {
            break;
        }

        // Check for stop sentinel
        let stop_path = paths.stop_sentinel(&config.group, config.thread_id.as_deref());
        if stop_path.exists() {
            tracing::info!("stop sentinel detected, ending phase 2 early");
            let _ = std::fs::remove_file(&stop_path);
            let status_path = paths.result_status(&config.session_id);
            std::fs::write(&status_path, ResultStatus::Stopped.as_str())
                .context("write status=stopped")?;
            return Ok(());
        }

        let pending = nq::list_pending(&queue_dir)?;

        if pending.is_empty() {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        for nq_path in &pending {
            // Read message
            let data = match nq::read_message(nq_path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(path = %nq_path.display(), error = %e, "failed to read nq file");
                    let _ = nq::delete(nq_path);
                    continue;
                }
            };

            // Parse as FollowUpMessage
            let followup: FollowUpMessage = match serde_json::from_slice(&data) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %nq_path.display(), error = %e, "invalid JSON in follow-up");
                    let _ = nq::delete(nq_path);
                    continue;
                }
            };

            // Skip empty text
            if followup.text.trim().is_empty() {
                tracing::warn!("follow-up has empty text, skipping");
                let _ = nq::delete(nq_path);
                continue;
            }

            let followup_prompt =
                format!("[Follow-up from {}]: {}", followup.sender, followup.text);

            tracing::info!(
                sender = %followup.sender,
                text_preview = %truncate(&followup.text, 80),
                "processing follow-up"
            );

            match claude::spawn_claude_resume(
                &config.claude_bin,
                &followup_prompt,
                config.max_turns,
                session_dir,
                &response_path,
                &log_path,
            ) {
                Ok(result) if result.exit_code != 0 => {
                    tracing::warn!(
                        exit_code = result.exit_code,
                        "claude --resume exited with error"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to spawn claude --resume");
                }
                _ => {}
            }

            // Check for context overflow after each resume
            if detect_context_overflow(&response_path, &log_path) {
                tracing::warn!("context overflow detected during follow-up");
                let _ = nq::delete(nq_path);
                let status_path = paths.result_status(&config.session_id);
                std::fs::write(&status_path, ResultStatus::Overflow.as_str())
                    .context("write status=overflow")?;
                return Ok(());
            }

            let _ = nq::delete(nq_path);
            followup_count += 1;
            tracing::info!(count = followup_count, "follow-up processed");
        }

        // Reset idle timer after processing messages
        last_activity = Instant::now();
    }

    tracing::info!(followup_count, "phase 2 complete: idle timeout reached");
    Ok(())
}

fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Detect context window overflow by scanning response.jsonl and agent.log.
fn detect_context_overflow(response_path: &Path, log_path: &Path) -> bool {
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

// ---------------------------------------------------------------------------
// Phase 3: Write final status
// ---------------------------------------------------------------------------

pub fn phase_3_done(paths: &DataPaths, session_id: &str) -> Result<()> {
    tracing::info!("phase 3: writing final status");
    let status_path = paths.result_status(session_id);
    std::fs::write(&status_path, ResultStatus::Done.as_str()).context("write status=done")?;
    tracing::info!("agent job complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_both_contexts() {
        let result = build_prompt(Some("soul"), Some("group"), "hello");
        assert_eq!(result, "soul\n\n---\n\ngroup\n\n---\n\nhello");
    }

    #[test]
    fn build_prompt_soul_only() {
        let result = build_prompt(Some("soul"), None, "hello");
        assert_eq!(result, "soul\n\n---\n\nhello");
    }

    #[test]
    fn build_prompt_group_only() {
        let result = build_prompt(None, Some("group"), "hello");
        assert_eq!(result, "group\n\n---\n\nhello");
    }

    #[test]
    fn build_prompt_no_context() {
        let result = build_prompt(None, None, "hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn build_prompt_empty_contexts_ignored() {
        let result = build_prompt(Some(""), Some(""), "hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn extract_session_id_from_result_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}
{"type":"result","result":"hello","session_id":"sess-abc-123"}
"#,
        )
        .unwrap();
        assert_eq!(
            extract_claude_session_id(&path),
            Some("sess-abc-123".to_string())
        );
    }

    #[test]
    fn extract_session_id_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        std::fs::write(&path, r#"{"type":"result","result":"hello"}"#).unwrap();
        assert_eq!(extract_claude_session_id(&path), None);
    }

    #[test]
    fn extract_session_id_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(extract_claude_session_id(&path), None);
    }

    #[test]
    fn extract_session_id_no_file() {
        let path = Path::new("/nonexistent/response.jsonl");
        assert_eq!(extract_claude_session_id(path), None);
    }

    #[test]
    fn detect_overflow_in_response() {
        let dir = tempfile::tempdir().unwrap();
        let resp = dir.path().join("response.jsonl");
        let log = dir.path().join("agent.log");
        std::fs::write(
            &resp,
            r#"{"type":"error","error":"context_length_exceeded"}"#,
        )
        .unwrap();
        std::fs::write(&log, "").unwrap();
        assert!(detect_context_overflow(&resp, &log));
    }

    #[test]
    fn detect_overflow_in_log() {
        let dir = tempfile::tempdir().unwrap();
        let resp = dir.path().join("response.jsonl");
        let log = dir.path().join("agent.log");
        std::fs::write(&resp, "").unwrap();
        std::fs::write(&log, "Error: maximum context length exceeded").unwrap();
        assert!(detect_context_overflow(&resp, &log));
    }

    #[test]
    fn no_overflow_normal_response() {
        let dir = tempfile::tempdir().unwrap();
        let resp = dir.path().join("response.jsonl");
        let log = dir.path().join("agent.log");
        std::fs::write(&resp, r#"{"type":"result","result":"hello"}"#).unwrap();
        std::fs::write(&log, "claude started\nclaude finished").unwrap();
        assert!(!detect_context_overflow(&resp, &log));
    }
}
