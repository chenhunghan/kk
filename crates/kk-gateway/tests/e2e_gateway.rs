mod common;

use std::collections::HashMap;
use std::io::Write;

use common::{GatewayTestHarness, always_group, sample_inbound};
use kk_core::nq;
use kk_core::types::{ChannelMapping, GroupEntry, GroupsConfig, TriggerMode};

// ──────────────────────────────────────────────
// Full Lifecycle (Results Loop)
// ──────────────────────────────────────────────

#[tokio::test]
async fn full_lifecycle() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;
    h.simulate_agent_done(&sid, "The answer is 42.");

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].text, "The answer is 42.");
    assert_eq!(outbound[0].channel, "test-channel");
    assert_eq!(outbound[0].group, "eng");
    assert!(outbound[0].thread_id.is_none());

    assert!(h.list_archived().contains(&sid));
    assert_eq!(h.active_job_count().await, 0);
}

#[tokio::test]
async fn full_lifecycle_threaded() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let mut msg = sample_inbound("eng", "hello");
    msg.thread_id = Some("42".to_string());

    let sid = h.simulate_cold_path(&msg).await;
    h.simulate_agent_done(&sid, "threaded reply");

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].text, "threaded reply");
    assert_eq!(outbound[0].thread_id.as_deref(), Some("42"));

    // Routing key eng|42 should be cleared
    let active = h.state.active_jobs.read().await;
    assert!(!active.contains_key("eng|42"));
}

#[tokio::test]
async fn error_result() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;
    h.simulate_agent_error(&sid);

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert!(outbound[0].text.contains("[kk] Agent job failed"));

    assert!(h.list_archived().contains(&sid));
    assert_eq!(h.active_job_count().await, 0);
}

#[tokio::test]
async fn no_response_file() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;
    // Set status=done but don't create response.jsonl
    std::fs::write(h.paths.result_status(&sid), "done").unwrap();

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].text, "(no response file)");
}

#[tokio::test]
async fn empty_response_file() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;
    // Write empty response.jsonl
    std::fs::write(h.paths.result_response(&sid), "").unwrap();
    std::fs::write(h.paths.result_status(&sid), "done").unwrap();

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].text, "(no response)");
}

#[tokio::test]
async fn assistant_fallback() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;

    // Write response.jsonl with only assistant lines (no "result" line)
    let response_path = h.paths.result_response(&sid);
    let mut f = std::fs::File::create(&response_path).unwrap();
    writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"Here is my answer."}}]}}}}"#).unwrap();
    std::fs::write(h.paths.result_status(&sid), "done").unwrap();

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].text, "Here is my answer.");
}

#[tokio::test]
async fn running_status_skipped() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "hello");

    let sid = h.simulate_cold_path(&msg).await;
    // Status is already "running" from simulate_cold_path — don't change it

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    // No outbound, not archived, active job still present
    let outbound = h.read_outbound("test-channel");
    assert!(outbound.is_empty());
    assert!(!h.list_archived().contains(&sid));
    assert_eq!(h.active_job_count().await, 1);
}

#[tokio::test]
async fn multiple_sessions_one_poll() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    let mut msg1 = sample_inbound("eng", "q1");
    msg1.timestamp -= 2; // slightly earlier
    let mut msg2 = sample_inbound("eng", "q2");
    msg2.thread_id = Some("99".to_string());

    let sid1 = h.simulate_cold_path(&msg1).await;
    let sid2 = h.simulate_cold_path(&msg2).await;

    h.simulate_agent_done(&sid1, "answer1");
    h.simulate_agent_done(&sid2, "answer2");

    kk_gateway::loops::results::poll_once(&h.state)
        .await
        .unwrap();

    let outbound = h.read_outbound("test-channel");
    assert_eq!(outbound.len(), 2);

    let texts: Vec<&str> = outbound.iter().map(|o| o.text.as_str()).collect();
    assert!(texts.contains(&"answer1"));
    assert!(texts.contains(&"answer2"));

    assert_eq!(h.active_job_count().await, 0);
    assert_eq!(h.list_archived().len(), 2);
}

// ──────────────────────────────────────────────
// Inbound Routing (Hot Path)
// ──────────────────────────────────────────────

#[tokio::test]
async fn hot_path_follow_up() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "initial");

    let sid = h.simulate_cold_path(&msg).await;

    // Now send a follow-up via inbound
    let follow_up = sample_inbound("eng", "follow up message");
    h.enqueue_inbound(&follow_up);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    // Should have a follow-up in the group queue
    let follow_ups = h.read_follow_ups("eng", None);
    assert_eq!(follow_ups.len(), 1);
    assert_eq!(follow_ups[0].text, "follow up message");

    // Manifest should be updated with new message
    let manifest = h.read_manifest(&sid);
    assert_eq!(manifest.messages.len(), 2);
    assert_eq!(manifest.messages[1].text, "follow up message");
}

#[tokio::test]
async fn hot_path_threaded() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let mut msg = sample_inbound("eng", "initial");
    msg.thread_id = Some("42".to_string());

    h.simulate_cold_path(&msg).await;

    // Send follow-up in same thread
    let mut follow_up = sample_inbound("eng", "thread follow up");
    follow_up.thread_id = Some("42".to_string());
    h.enqueue_inbound(&follow_up);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    // Follow-up should be in threads/42/ subdir
    let follow_ups = h.read_follow_ups("eng", Some("42"));
    assert_eq!(follow_ups.len(), 1);
    assert_eq!(follow_ups[0].text, "thread follow up");
}

#[tokio::test]
async fn trigger_always() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "any message");
    let _sid = h.simulate_cold_path(&msg).await;

    let msg2 = sample_inbound("eng", "anything goes");
    h.enqueue_inbound(&msg2);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert_eq!(follow_ups.len(), 1);
}

#[tokio::test]
async fn trigger_mention_match() {
    let group = GroupEntry {
        trigger_mode: TriggerMode::Mention,
        trigger_pattern: Some("@bot".to_string()),
        channels: HashMap::from([(
            "test-channel".to_string(),
            ChannelMapping {
                chat_id: "-100".to_string(),
            },
        )]),
    };
    let h = GatewayTestHarness::with_groups(groups_with("eng", group)).await;

    let msg = sample_inbound("eng", "initial");
    let _sid = h.simulate_cold_path(&msg).await;

    let msg2 = sample_inbound("eng", "hey @bot what's up?");
    h.enqueue_inbound(&msg2);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert_eq!(follow_ups.len(), 1);
}

#[tokio::test]
async fn trigger_mention_no_match() {
    let group = GroupEntry {
        trigger_mode: TriggerMode::Mention,
        trigger_pattern: Some("@bot".to_string()),
        channels: HashMap::from([(
            "test-channel".to_string(),
            ChannelMapping {
                chat_id: "-100".to_string(),
            },
        )]),
    };
    let h = GatewayTestHarness::with_groups(groups_with("eng", group)).await;

    let msg = sample_inbound("eng", "initial");
    let _sid = h.simulate_cold_path(&msg).await;

    // Message without "@bot" — should NOT trigger
    let msg2 = sample_inbound("eng", "hey what's up?");
    h.enqueue_inbound(&msg2);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert!(follow_ups.is_empty());
}

#[tokio::test]
async fn trigger_prefix_match() {
    let group = GroupEntry {
        trigger_mode: TriggerMode::Prefix,
        trigger_pattern: Some("/ask".to_string()),
        channels: HashMap::from([(
            "test-channel".to_string(),
            ChannelMapping {
                chat_id: "-100".to_string(),
            },
        )]),
    };
    let h = GatewayTestHarness::with_groups(groups_with("eng", group)).await;

    let msg = sample_inbound("eng", "/ask initial");
    let _sid = h.simulate_cold_path(&msg).await;

    let msg2 = sample_inbound("eng", "/ask what is rust?");
    h.enqueue_inbound(&msg2);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert_eq!(follow_ups.len(), 1);
}

#[tokio::test]
async fn trigger_prefix_no_match() {
    let group = GroupEntry {
        trigger_mode: TriggerMode::Prefix,
        trigger_pattern: Some("/ask".to_string()),
        channels: HashMap::from([(
            "test-channel".to_string(),
            ChannelMapping {
                chat_id: "-100".to_string(),
            },
        )]),
    };
    let h = GatewayTestHarness::with_groups(groups_with("eng", group)).await;

    let msg = sample_inbound("eng", "/ask initial");
    let _sid = h.simulate_cold_path(&msg).await;

    // Message without "/ask" prefix — should NOT trigger
    let msg2 = sample_inbound("eng", "what is rust?");
    h.enqueue_inbound(&msg2);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert!(follow_ups.is_empty());
}

#[tokio::test]
async fn unregistered_group_skipped() {
    // Empty groups config — no groups registered
    let h = GatewayTestHarness::new().await;

    let msg = sample_inbound("unknown-group", "hello");
    h.enqueue_inbound(&msg);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    // Message consumed but no routing — inbound dir should be empty
    let pending = nq::list_pending(&h.paths.inbound_dir()).unwrap();
    assert!(pending.is_empty());

    // No follow-ups anywhere
    let follow_ups = h.read_follow_ups("unknown-group", None);
    assert!(follow_ups.is_empty());
}

#[tokio::test]
async fn stale_message_discarded() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    let mut msg = sample_inbound("eng", "old message");
    // Set timestamp to 600 seconds ago (stale_timeout is 300)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    msg.timestamp = now - 600;

    h.enqueue_inbound(&msg);

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    // Message consumed (deleted) but no routing
    let pending = nq::list_pending(&h.paths.inbound_dir()).unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn invalid_json_discarded() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    // Write malformed JSON to inbound
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    nq::enqueue(&h.paths.inbound_dir(), now, b"not valid json{{{").unwrap();

    // Should not panic
    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    // File consumed (deleted)
    let pending = nq::list_pending(&h.paths.inbound_dir()).unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn multiple_messages_one_poll() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;
    let msg = sample_inbound("eng", "initial");
    let _sid = h.simulate_cold_path(&msg).await;

    // Enqueue 3 follow-ups
    for i in 0..3 {
        let mut m = sample_inbound("eng", &format!("msg {i}"));
        m.timestamp += i as u64; // ensure unique timestamps
        h.enqueue_inbound(&m);
    }

    kk_gateway::loops::inbound::poll_once(&h.state)
        .await
        .unwrap();

    let follow_ups = h.read_follow_ups("eng", None);
    assert_eq!(follow_ups.len(), 3);
}

// ──────────────────────────────────────────────
// Groups Config
// ──────────────────────────────────────────────

#[tokio::test]
async fn groups_d_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = kk_core::paths::DataPaths::new(tmp.path());
    paths.ensure_dirs().unwrap();

    // Write two groups.d files
    let groups_d = paths.groups_d_dir();
    std::fs::write(
        groups_d.join("telegram.json"),
        r#"{"groups":{"family":{"trigger_mode":"always","channels":{"tg":{"chat_id":"-100"}}}}}"#,
    )
    .unwrap();
    std::fs::write(
        groups_d.join("slack.json"),
        r#"{"groups":{"work":{"trigger_mode":"prefix","trigger_pattern":"/ask","channels":{"slack":{"chat_id":"C123"}}}}}"#,
    )
    .unwrap();

    // Write groups.json overlay — overrides "family" trigger_mode
    std::fs::write(
        paths.groups_json(),
        r#"{"groups":{"family":{"trigger_mode":"mention","trigger_pattern":"@bot","channels":{"tg":{"chat_id":"-100"}}}}}"#,
    )
    .unwrap();

    // Construct state from these paths — this tests the real load_groups_config
    let config = kk_gateway::config::GatewayConfig {
        data_dir: tmp.path().to_string_lossy().to_string(),
        namespace: "test".to_string(),
        image_agent: "kk-agent:test".to_string(),
        api_keys_secret: "kk-api-keys".to_string(),
        job_active_deadline: 300,
        job_ttl_after_finished: 300,
        job_idle_timeout: 120,
        job_max_turns: 25,
        job_cpu_request: "250m".to_string(),
        job_cpu_limit: "1".to_string(),
        job_memory_request: "256Mi".to_string(),
        job_memory_limit: "1Gi".to_string(),
        inbound_poll_interval_ms: 100,
        results_poll_interval_ms: 100,
        cleanup_interval_ms: 100,
        stale_message_timeout: 300,
        results_archive_ttl: 86400,
        pvc_claim_name: "kk-data".to_string(),
        state_reload_interval_ms: 30000,
        agent_type: "claude".to_string(),
    };

    let launcher: std::sync::Arc<dyn kk_gateway::launcher::Launcher> =
        std::sync::Arc::new(common::MockLauncher::new());

    let state = kk_gateway::state::SharedState::new(config, launcher, &paths).unwrap();
    let groups = state.groups_config.read().await;

    // Both groups should be present
    assert!(groups.groups.contains_key("family"));
    assert!(groups.groups.contains_key("work"));
    assert_eq!(groups.groups.len(), 2);

    // Admin override should win for "family"
    assert_eq!(groups.groups["family"].trigger_mode, TriggerMode::Mention);
    assert_eq!(
        groups.groups["family"].trigger_pattern.as_deref(),
        Some("@bot")
    );

    // "work" comes from groups.d only
    assert_eq!(groups.groups["work"].trigger_mode, TriggerMode::Prefix);
}

// ──────────────────────────────────────────────
// Cleanup (File-Based)
// ──────────────────────────────────────────────

#[tokio::test]
async fn orphaned_queue_cleanup() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    // Create a queue file in eng group dir
    let queue_dir = h.paths.group_queue_dir("eng");
    std::fs::create_dir_all(&queue_dir).unwrap();
    let file_path = nq::enqueue(&queue_dir, 1000, b"old follow-up").unwrap();

    // Backdate the file to be older than stale_message_timeout (300s)
    let old_time = filetime::FileTime::from_unix_time(0, 0);
    filetime::set_file_mtime(&file_path, old_time).unwrap();

    kk_gateway::loops::cleanup::cleanup_orphaned_queues(&h.state).unwrap();

    // File should be deleted
    let pending = nq::list_pending(&queue_dir).unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn orphaned_thread_queue_cleanup() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    // Create a queue file in eng/threads/42/ subdir
    let thread_dir = h.paths.group_queue_dir_threaded("eng", Some("42"));
    std::fs::create_dir_all(&thread_dir).unwrap();
    let file_path = nq::enqueue(&thread_dir, 1000, b"old thread follow-up").unwrap();

    // Backdate the file
    let old_time = filetime::FileTime::from_unix_time(0, 0);
    filetime::set_file_mtime(&file_path, old_time).unwrap();

    kk_gateway::loops::cleanup::cleanup_orphaned_queues(&h.state).unwrap();

    // File should be deleted
    let pending = nq::list_pending(&thread_dir).unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn archive_purge() {
    let h = GatewayTestHarness::with_groups(groups_with("eng", always_group())).await;

    // Create an old archived result dir
    let done_dir = h.paths.results_done_dir();
    std::fs::create_dir_all(&done_dir).unwrap();
    let old_archive = done_dir.join("old-session-123");
    std::fs::create_dir_all(&old_archive).unwrap();
    std::fs::write(old_archive.join("request.json"), "{}").unwrap();

    // Backdate the directory mtime
    let old_time = filetime::FileTime::from_unix_time(0, 0);
    filetime::set_file_mtime(&old_archive, old_time).unwrap();

    kk_gateway::loops::cleanup::purge_old_archives(&h.state).unwrap();

    // Archive dir should be purged
    assert!(!old_archive.exists());
}

// ──────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────

fn groups_with(slug: &str, entry: GroupEntry) -> GroupsConfig {
    GroupsConfig {
        groups: HashMap::from([(slug.to_string(), entry)]),
    }
}
