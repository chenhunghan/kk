use std::fs;
use std::path::{Path, PathBuf};

use kk_agent::config::AgentConfig;
use kk_agent::phases;
use kk_core::nq;
use kk_core::paths::DataPaths;
use kk_core::types::{FollowUpMessage, RequestManifest, RequestMessage};

// ===========================================================================
// Helpers
// ===========================================================================

/// Create a mock claude binary that writes predictable JSONL and exits with the given code.
/// The mock also writes the prompt argument ($2) to `.last-prompt` in cwd.
fn create_mock_claude(dir: &Path, exit_code: i32) -> PathBuf {
    let script = dir.join("mock-claude");
    let content = format!(
        r#"#!/bin/bash
echo "$2" > .last-prompt
echo '{{"type":"result","result":"mock response"}}'
exit {exit_code}
"#
    );
    fs::write(&script, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

fn make_config(data_dir: &Path, agent_bin: &Path) -> AgentConfig {
    AgentConfig {
        session_id: "test-group-1234".to_string(),
        group: "test-group".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        idle_timeout: 1,
        max_turns: 5,
        thread_id: None,
        agent_type: kk_agent::config::AgentType::Claude,
        agent_bin: agent_bin.to_string_lossy().into_owned(),
    }
}

fn write_request_manifest(paths: &DataPaths, session_id: &str, group: &str, prompt: &str) {
    let manifest = RequestManifest {
        channel: "test-channel".to_string(),
        group: group.to_string(),
        thread_id: None,
        sender: "tester".to_string(),
        meta: serde_json::Value::Object(Default::default()),
        messages: vec![RequestMessage {
            sender: "tester".to_string(),
            text: prompt.to_string(),
            ts: 1234,
        }],
    };
    let manifest_path = paths.request_manifest(session_id);
    fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
}

struct TestEnv {
    _tmp: tempfile::TempDir,
    paths: DataPaths,
    config: AgentConfig,
    session_dir: PathBuf,
}

fn setup(exit_code: i32) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let paths = DataPaths::new(&data_dir);
    paths.ensure_dirs().unwrap();

    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let mock_bin = create_mock_claude(&bin_dir, exit_code);

    let config = make_config(&data_dir, &mock_bin);

    // Create results dir and session dir
    fs::create_dir_all(paths.results_dir(&config.session_id)).unwrap();
    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    fs::create_dir_all(&session_dir).unwrap();

    TestEnv {
        _tmp: tmp,
        paths,
        config,
        session_dir,
    }
}

fn enqueue_followup(queue_dir: &Path, sender: &str, text: &str) {
    let msg = FollowUpMessage {
        sender: sender.to_string(),
        text: text.to_string(),
        timestamp: 9999,
        channel: "test-channel".to_string(),
        thread_id: None,
        meta: serde_json::Value::Object(Default::default()),
    };
    let payload = serde_json::to_vec(&msg).unwrap();
    nq::enqueue(queue_dir, 9999, &payload).unwrap();
}

// ===========================================================================
// Phase 0 — Skills
// ===========================================================================

#[test]
fn phase0_skills_injected() {
    let env = setup(0);

    // Create a valid skill
    let skill_dir = env.paths.skill_dir("my-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), "# My Skill").unwrap();

    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();

    // Verify .claude/skills/ symlink
    let claude_link = env.session_dir.join(".claude/skills/my-skill");
    assert!(claude_link.exists(), ".claude/skills/my-skill should exist");
    let target = fs::read_link(&claude_link).unwrap();
    assert_eq!(target, skill_dir);

    // Verify .agents/skills/ symlink (chained)
    let agents_link = env.session_dir.join(".agents/skills/my-skill");
    assert!(agents_link.exists(), ".agents/skills/my-skill should exist");
    let agents_target = fs::read_link(&agents_link).unwrap();
    assert_eq!(agents_target, claude_link);
}

#[test]
fn phase0_missing_skill_md_skipped() {
    let env = setup(0);

    // Create skill dir without SKILL.md
    let skill_dir = env.paths.skill_dir("broken-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("README.md"), "not a skill").unwrap();

    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();

    let link = env.session_dir.join(".claude/skills/broken-skill");
    assert!(!link.exists(), "broken skill should not be symlinked");
}

#[test]
fn phase0_no_skills_dir() {
    let env = setup(0);

    // Remove the skills dir created by ensure_dirs
    let _ = fs::remove_dir(&env.paths.skills_dir());

    // Should succeed with no skills
    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();
}

#[test]
fn phase0_existing_symlink_overwritten() {
    let env = setup(0);

    // Create initial skill and run phase 0
    let skill_v1 = env.paths.skill_dir("evolving-skill");
    fs::create_dir_all(&skill_v1).unwrap();
    fs::write(skill_v1.join("SKILL.md"), "# v1").unwrap();
    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();

    let claude_link = env.session_dir.join(".claude/skills/evolving-skill");
    let target_v1 = fs::read_link(&claude_link).unwrap();
    assert_eq!(target_v1, skill_v1);

    // "Update" skill by removing and recreating with different content
    // (In practice the controller would do this)
    fs::write(skill_v1.join("SKILL.md"), "# v2 updated").unwrap();

    // Run phase 0 again — should overwrite symlink cleanly
    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();

    let target_v2 = fs::read_link(&claude_link).unwrap();
    assert_eq!(target_v2, skill_v1);
    // Verify we can read through the symlink
    let content = fs::read_to_string(claude_link.join("SKILL.md")).unwrap();
    assert_eq!(content, "# v2 updated");
}

// ===========================================================================
// Phase 1 — Initial Prompt
// ===========================================================================

#[test]
fn phase1_success() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello world",
    );

    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    // Status should still be "running" (phase 3 writes "done")
    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "running");

    // Response file should have content from mock
    let response = fs::read_to_string(env.paths.result_response(&env.config.session_id)).unwrap();
    assert!(response.contains(r#""type":"result""#));
}

#[test]
fn phase1_fatal_error() {
    let env = setup(1); // mock exits with code 1
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello",
    );

    let result = phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir);
    assert!(result.is_err(), "phase 1 should fail on exit code 1");

    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "error");
}

#[test]
fn phase1_nonfatal_error() {
    let env = setup(2); // mock exits with code 2 (non-fatal)
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello",
    );

    // Should succeed (non-fatal error continues)
    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "running");
}

#[test]
fn phase1_context_both() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello",
    );

    // Write both context files
    let soul_path = env.paths.soul_md();
    fs::create_dir_all(soul_path.parent().unwrap()).unwrap();
    fs::write(&soul_path, "You are a helpful AI").unwrap();

    let group_md = env.paths.group_claude_md(&env.config.group);
    fs::create_dir_all(group_md.parent().unwrap()).unwrap();
    fs::write(&group_md, "This is the test group").unwrap();

    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    // Verify prompt passed to mock includes all parts
    let prompt = fs::read_to_string(env.session_dir.join(".last-prompt")).unwrap();
    assert!(
        prompt.contains("You are a helpful AI"),
        "prompt should contain SOUL.md"
    );
    assert!(
        prompt.contains("This is the test group"),
        "prompt should contain CLAUDE.md"
    );
    assert!(
        prompt.contains("hello"),
        "prompt should contain user message"
    );
    assert!(prompt.contains("---"), "prompt should contain separators");
}

#[test]
fn phase1_context_soul_only() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello",
    );

    let soul_path = env.paths.soul_md();
    fs::create_dir_all(soul_path.parent().unwrap()).unwrap();
    fs::write(&soul_path, "You are a helpful AI").unwrap();

    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    let prompt = fs::read_to_string(env.session_dir.join(".last-prompt")).unwrap();
    assert!(prompt.contains("You are a helpful AI"));
    assert!(prompt.contains("hello"));
}

#[test]
fn phase1_context_none() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello",
    );

    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    let prompt = fs::read_to_string(env.session_dir.join(".last-prompt")).unwrap();
    // Prompt should be just the user message (no separators)
    assert_eq!(prompt.trim(), "hello");
}

#[test]
fn phase1_reads_prompt_from_request_json() {
    let env = setup(0);
    let specific_prompt = "what is the capital of Finland?";
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        specific_prompt,
    );

    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    let prompt = fs::read_to_string(env.session_dir.join(".last-prompt")).unwrap();
    assert!(
        prompt.contains(specific_prompt),
        "prompt should come from request.json"
    );
}

// ===========================================================================
// Phase 2 — Follow-ups
// ===========================================================================

#[test]
fn phase2_followup_processed() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "init",
    );

    // Run phase 1 first to create response.jsonl
    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    // Enqueue a follow-up
    let queue_dir = env
        .paths
        .group_queue_dir_threaded(&env.config.group, env.config.thread_id.as_deref());
    enqueue_followup(&queue_dir, "John", "also check my calendar");

    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();

    // Follow-up nq file should be deleted
    let pending = nq::list_pending(&queue_dir).unwrap();
    assert!(
        pending.is_empty(),
        "nq file should be deleted after processing"
    );

    // .last-prompt should contain the follow-up prompt (overwrites phase 1's)
    let prompt = fs::read_to_string(env.session_dir.join(".last-prompt")).unwrap();
    assert!(prompt.contains("[Follow-up from John]"));
    assert!(prompt.contains("also check my calendar"));

    // response.jsonl should have content from both runs
    let response = fs::read_to_string(env.paths.result_response(&env.config.session_id)).unwrap();
    let lines: Vec<&str> = response.lines().collect();
    assert!(lines.len() >= 2, "should have lines from initial + resume");
}

#[test]
fn phase2_empty_text_skipped() {
    let env = setup(0);

    let queue_dir = env
        .paths
        .group_queue_dir_threaded(&env.config.group, env.config.thread_id.as_deref());
    // Enqueue with empty text
    let msg = FollowUpMessage {
        sender: "John".to_string(),
        text: "  ".to_string(), // whitespace-only
        timestamp: 9999,
        channel: "test".to_string(),
        thread_id: None,
        meta: serde_json::Value::Object(Default::default()),
    };
    nq::enqueue(&queue_dir, 9999, &serde_json::to_vec(&msg).unwrap()).unwrap();

    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();

    // nq file should be deleted
    assert!(nq::list_pending(&queue_dir).unwrap().is_empty());
    // Mock should NOT have been called
    assert!(
        !env.session_dir.join(".last-prompt").exists(),
        "mock should not be called for empty text"
    );
}

#[test]
fn phase2_invalid_json_skipped() {
    let env = setup(0);

    let queue_dir = env
        .paths
        .group_queue_dir_threaded(&env.config.group, env.config.thread_id.as_deref());
    // Enqueue invalid JSON
    nq::enqueue(&queue_dir, 9999, b"not valid json {{{").unwrap();

    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();

    // nq file should be deleted
    assert!(nq::list_pending(&queue_dir).unwrap().is_empty());
    // Mock should NOT have been called
    assert!(!env.session_dir.join(".last-prompt").exists());
}

#[test]
fn phase2_idle_timeout() {
    let env = setup(0);

    let start = std::time::Instant::now();
    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();
    let elapsed = start.elapsed();

    // With idle_timeout=1 and poll_interval=2, should take ~2s (one sleep cycle)
    assert!(elapsed.as_secs() >= 1, "should wait at least idle_timeout");
    assert!(elapsed.as_secs() < 10, "should not wait excessively");
}

#[test]
fn phase2_threaded_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let paths = DataPaths::new(&data_dir);
    paths.ensure_dirs().unwrap();

    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let mock_bin = create_mock_claude(&bin_dir, 0);

    let config = AgentConfig {
        session_id: "test-group-t42-1234".to_string(),
        group: "test-group".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        idle_timeout: 1,
        max_turns: 5,
        thread_id: Some("42".to_string()),
        agent_type: kk_agent::config::AgentType::Claude,
        agent_bin: mock_bin.to_string_lossy().into_owned(),
    };

    fs::create_dir_all(paths.results_dir(&config.session_id)).unwrap();
    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    fs::create_dir_all(&session_dir).unwrap();

    // Create response.jsonl for append
    fs::write(paths.result_response(&config.session_id), "").unwrap();
    fs::write(paths.results_dir(&config.session_id).join("agent.log"), "").unwrap();

    // Enqueue to thread-specific queue
    let queue_dir = paths.group_queue_dir_threaded("test-group", Some("42"));
    enqueue_followup(&queue_dir, "Sarah", "thread reply");

    phases::phase_2_followups(&config, &paths, &session_dir).unwrap();

    // Verify processed from thread queue
    assert!(nq::list_pending(&queue_dir).unwrap().is_empty());

    // Non-threaded queue should be unaffected
    let main_queue = paths.group_queue_dir("test-group");
    // main_queue either doesn't exist or is empty
    assert!(nq::list_pending(&main_queue).unwrap().is_empty());

    // Mock was called with follow-up prompt
    let prompt = fs::read_to_string(session_dir.join(".last-prompt")).unwrap();
    assert!(prompt.contains("[Follow-up from Sarah]"));
    assert!(prompt.contains("thread reply"));
}

// ===========================================================================
// Phase 3 — Done
// ===========================================================================

#[test]
fn phase3_status_done() {
    let env = setup(0);

    // Write initial status
    fs::write(env.paths.result_status(&env.config.session_id), "running").unwrap();

    phases::phase_3_done(&env.paths, &env.config.session_id).unwrap();

    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "done");
}

// ===========================================================================
// E2E — Full lifecycle
// ===========================================================================

#[test]
fn e2e_full_lifecycle() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "hello world",
    );

    // Create a skill
    let skill_dir = env.paths.skill_dir("test-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), "# Test").unwrap();

    // Phase 0
    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();
    assert!(env.session_dir.join(".claude/skills/test-skill").exists());

    // Phase 1
    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();
    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "running");

    // Phase 2 (no follow-ups, just idle timeout)
    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();

    // Phase 3
    phases::phase_3_done(&env.paths, &env.config.session_id).unwrap();
    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "done");

    // Response file should exist with content
    let response = fs::read_to_string(env.paths.result_response(&env.config.session_id)).unwrap();
    assert!(!response.is_empty());
}

#[test]
fn e2e_lifecycle_with_followup() {
    let env = setup(0);
    write_request_manifest(
        &env.paths,
        &env.config.session_id,
        &env.config.group,
        "initial prompt",
    );

    // Phase 0 (no skills)
    phases::phase_0_skills(&env.config, &env.paths, &env.session_dir).unwrap();

    // Phase 1
    phases::phase_1_prompt(&env.config, &env.paths, &env.session_dir).unwrap();

    // Enqueue follow-up before phase 2
    let queue_dir = env
        .paths
        .group_queue_dir_threaded(&env.config.group, env.config.thread_id.as_deref());
    enqueue_followup(&queue_dir, "Alice", "follow-up message");

    // Phase 2 (processes follow-up, then idle timeout)
    phases::phase_2_followups(&env.config, &env.paths, &env.session_dir).unwrap();

    // Follow-up should be consumed
    assert!(nq::list_pending(&queue_dir).unwrap().is_empty());

    // Phase 3
    phases::phase_3_done(&env.paths, &env.config.session_id).unwrap();
    let status = fs::read_to_string(env.paths.result_status(&env.config.session_id)).unwrap();
    assert_eq!(status, "done");

    // Response should have output from both phase 1 and phase 2
    let response = fs::read_to_string(env.paths.result_response(&env.config.session_id)).unwrap();
    let lines: Vec<&str> = response.lines().collect();
    assert!(
        lines.len() >= 2,
        "response should have lines from initial + follow-up, got {}",
        lines.len()
    );
}

#[test]
fn e2e_gemini_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let paths = DataPaths::new(&data_dir);
    paths.ensure_dirs().unwrap();

    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    
    // Create a mock gemini binary that also writes a session_id
    let script = bin_dir.join("mock-gemini");
    let content = r#"#!/bin/bash
echo "$@" > .last-args
echo '{"type":"result","result":"gemini response","session_id":"gemini-sess-999"}'
exit 0
"#;
    fs::write(&script, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let config = AgentConfig {
        session_id: "test-gemini-123".to_string(),
        group: "test-group".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        idle_timeout: 1,
        max_turns: 5,
        thread_id: None,
        agent_type: kk_agent::config::AgentType::Gemini,
        agent_bin: script.to_string_lossy().into_owned(),
    };

    fs::create_dir_all(paths.results_dir(&config.session_id)).unwrap();
    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    fs::create_dir_all(&session_dir).unwrap();

    write_request_manifest(&paths, &config.session_id, &config.group, "gemini prompt");

    // Phase 0
    phases::phase_0_skills(&config, &paths, &session_dir).unwrap();
    assert!(session_dir.join(".gemini/skills").exists());

    // Phase 1
    phases::phase_1_prompt(&config, &paths, &session_dir).unwrap();
    
    // Verify args
    let args = fs::read_to_string(session_dir.join(".last-args")).unwrap();
    assert!(args.contains("-p gemini prompt"));
    assert!(args.contains("--yolo"));
    assert!(args.contains("--output-format stream-json"));

    // Phase 2 (with follow-up)
    let queue_dir = paths.group_queue_dir_threaded(&config.group, config.thread_id.as_deref());
    enqueue_followup(&queue_dir, "Alice", "gemini follow-up");
    phases::phase_2_followups(&config, &paths, &session_dir).unwrap();

    // Verify resume args used the session_id from Phase 1
    let args = fs::read_to_string(session_dir.join(".last-args")).unwrap();
    assert!(args.contains("--resume gemini-sess-999"));
    assert!(args.contains("-p [Follow-up from Alice]: gemini follow-up"));

    // Phase 3
    phases::phase_3_done(&paths, &config.session_id).unwrap();
    let status = fs::read_to_string(paths.result_status(&config.session_id)).unwrap();
    assert_eq!(status, "done");
}

#[test]
fn e2e_codex_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let paths = DataPaths::new(&data_dir);
    paths.ensure_dirs().unwrap();

    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    
    // Create a mock codex binary
    let script = bin_dir.join("mock-codex");
    let content = r#"#!/bin/bash
echo "$@" > .last-args
echo '{"type":"result","result":"codex response","session_id":"codex-sess-123"}'
exit 0
"#;
    fs::write(&script, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let config = AgentConfig {
        session_id: "test-codex-123".to_string(),
        group: "test-group".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        idle_timeout: 1,
        max_turns: 5,
        thread_id: None,
        agent_type: kk_agent::config::AgentType::Codex,
        agent_bin: script.to_string_lossy().into_owned(),
    };

    fs::create_dir_all(paths.results_dir(&config.session_id)).unwrap();
    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    fs::create_dir_all(&session_dir).unwrap();

    write_request_manifest(&paths, &config.session_id, &config.group, "codex prompt");

    // Phase 0
    phases::phase_0_skills(&config, &paths, &session_dir).unwrap();
    assert!(session_dir.join(".codex/skills").exists());

    // Phase 1
    phases::phase_1_prompt(&config, &paths, &session_dir).unwrap();
    
    // Verify args
    let args = fs::read_to_string(session_dir.join(".last-args")).unwrap();
    assert!(args.contains("exec codex prompt"));
    assert!(args.contains("--json"));
    assert!(args.contains("--dangerously-bypass-approvals-and-sandbox"));

    // Phase 2 (with follow-up)
    let queue_dir = paths.group_queue_dir_threaded(&config.group, config.thread_id.as_deref());
    enqueue_followup(&queue_dir, "Alice", "codex follow-up");
    phases::phase_2_followups(&config, &paths, &session_dir).unwrap();

    // Verify resume args
    let args = fs::read_to_string(session_dir.join(".last-args")).unwrap();
    assert!(args.contains("exec resume codex-sess-123 [Follow-up from Alice]: codex follow-up"));

    // Phase 3
    phases::phase_3_done(&paths, &config.session_id).unwrap();
    let status = fs::read_to_string(paths.result_status(&config.session_id)).unwrap();
    assert_eq!(status, "done");
}
