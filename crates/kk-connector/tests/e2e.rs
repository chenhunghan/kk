//! End-to-end tests for kk-connector.
//!
//! Each test simulates a provider event (message, bot-added-to-group, etc.) and
//! verifies that the Connector writes the correct data to disk — nq files in
//! `/data/inbound/`, group mappings in `groups.d/{channel}.json`, and outbound
//! file lifecycle.
//!
//! ## Test matrix
//!
//! | Test | Trigger | Verified on disk |
//! |---|---|---|
//! | `e2e_inbound_lifecycle` | Message in known group | nq file with correct `InboundMessage` fields |
//! | `e2e_inbound_threaded` | Message in threaded group | nq file with `thread_id` set |
//! | `e2e_auto_register` | Message in unknown group | `groups.d/` mapping created + nq file with auto slug |
//! | `e2e_new_chat_event` | Bot added to group | `groups.d/` mapping created, no nq file |
//! | `e2e_auto_register_reuses_slug` | Two messages in unknown group | Both nq files share the same auto slug |
//! | `e2e_outbound_validation` | Outbound nq files | Malformed/mismatched deleted, valid retained |

use std::path::PathBuf;

use tokio::sync::mpsc;

use kk_connector::config::ConnectorConfig;
use kk_connector::groups::GroupMap;
use kk_connector::inbound::process_inbound;
use kk_connector::provider::{ConnectorEvent, InboundRaw};
use kk_core::nq;
use kk_core::types::{ChannelType, GroupsConfig, InboundMessage, OutboundMessage};

// -- External (provider) --
/// Provider-specific chat ID — opaque to kk internals.
const TEST_CHAT_ID: &str = "-1001234567890";

// -- Internal (kk) --
/// Group slug used across kk components (gateway, agent, etc.).
const TEST_GROUP_SLUG: &str = "dev-team";
/// Channel name — identifies this connector instance, matches a Channel CRD.
const TEST_CHANNEL: &str = "telegram-bot-channel";

/// Common test scaffold: temp dirs, config, and group map.
struct TestHarness {
    config: ConnectorConfig,
    group_map: GroupMap,
    inbound_dir: PathBuf,
    groups_d_file: PathBuf,
    _tmp: tempfile::TempDir, // held to keep dirs alive
}

impl TestHarness {
    /// Create a harness with no pre-existing group mappings.
    fn empty() -> Self {
        Self::new(None)
    }

    /// Create a harness with a single pre-configured chat_id → slug mapping.
    fn with_mapping(chat_id: &str, slug: &str) -> Self {
        Self::new(Some((chat_id, slug)))
    }

    fn new(mapping: Option<(&str, &str)>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let inbound_dir = tmp.path().join("inbound");
        let groups_d_dir = tmp.path().join("state").join("groups.d");
        std::fs::create_dir_all(&inbound_dir).unwrap();
        std::fs::create_dir_all(&groups_d_dir).unwrap();

        let groups_d_file = groups_d_dir.join(format!("{TEST_CHANNEL}.json"));

        if let Some((chat_id, slug)) = mapping {
            write_groups_config(&groups_d_file, chat_id, slug, TEST_CHANNEL);
        }

        let config = ConnectorConfig {
            channel_type: "telegram".into(),
            channel_name: TEST_CHANNEL.into(),
            inbound_dir: inbound_dir.to_str().unwrap().into(),
            outbox_dir: tmp.path().join("outbox").to_str().unwrap().into(),
            groups_d_file: groups_d_file.to_str().unwrap().into(),
            telegram_bot_token: None,
            outbound_poll_interval_ms: 1_000,
        };

        let group_map = GroupMap::load(&config.groups_d_file, &config.channel_name);

        Self {
            config,
            group_map,
            inbound_dir,
            groups_d_file,
            _tmp: tmp,
        }
    }

    /// Send events through process_inbound and wait for completion.
    async fn process(&mut self, events: Vec<ConnectorEvent>) {
        let (tx, rx) = mpsc::channel::<ConnectorEvent>(16);
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        // Clone the group_map so process_inbound can own it; replace ours with
        // the loaded-from-disk version after processing (picks up persisted state).
        let gm = self.group_map.clone();
        process_inbound(rx, &self.config, gm).await;
        // Reload from disk to reflect any persisted auto-registrations.
        self.group_map = GroupMap::load(&self.config.groups_d_file, &self.config.channel_name);
    }

    /// Read all pending inbound nq messages.
    fn read_inbound(&self) -> Vec<InboundMessage> {
        nq::list_pending(&self.inbound_dir)
            .unwrap()
            .iter()
            .map(|p| serde_json::from_slice(&nq::read_message(p).unwrap()).unwrap())
            .collect()
    }

    /// Read the persisted groups.d config file.
    fn read_groups_config(&self) -> GroupsConfig {
        serde_json::from_str(&std::fs::read_to_string(&self.groups_d_file).unwrap()).unwrap()
    }
}

/// Write a groups config file with a single chat_id → group mapping.
fn write_groups_config(path: &PathBuf, chat_id: &str, group: &str, channel: &str) {
    let json = serde_json::json!({
        "groups": {
            group: {
                "trigger_mode": "always",
                "channels": {
                    channel: { "chat_id": chat_id }
                }
            }
        }
    });
    std::fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
}

/// Message in a known group → nq file written to `/data/inbound/` with correct
/// `InboundMessage` fields (group slug, sender, text, timestamp, provider metadata).
#[tokio::test]
async fn e2e_inbound_lifecycle() {
    let mut h = TestHarness::with_mapping(TEST_CHAT_ID, TEST_GROUP_SLUG);

    h.process(vec![ConnectorEvent::Message(InboundRaw {
        chat_id: TEST_CHAT_ID.into(),
        sender_name: "Alice".into(),
        text: "hello world".into(),
        timestamp: 1700000000,
        thread_id: None,
        meta: serde_json::json!({
            "chat_id": TEST_CHAT_ID,
            "message_id": 42,
        }),
    })])
    .await;

    let msgs = h.read_inbound();
    assert_eq!(msgs.len(), 1, "expected exactly one nq file");

    let msg = &msgs[0];
    assert_eq!(msg.channel, TEST_CHANNEL);
    assert_eq!(msg.channel_type, ChannelType::Telegram);
    assert_eq!(msg.group, TEST_GROUP_SLUG);
    assert_eq!(msg.thread_id, None);
    assert_eq!(msg.sender, "Alice");
    assert_eq!(msg.text, "hello world");
    assert_eq!(msg.timestamp, 1700000000);
    assert_eq!(msg.meta["chat_id"], TEST_CHAT_ID);
    assert_eq!(msg.meta["message_id"], 42);
}

/// Message in a threaded group → nq file has `thread_id` set so the Gateway can
/// route it to a thread-specific agent job.
#[tokio::test]
async fn e2e_inbound_threaded() {
    let mut h = TestHarness::with_mapping(TEST_CHAT_ID, TEST_GROUP_SLUG);

    h.process(vec![ConnectorEvent::Message(InboundRaw {
        chat_id: TEST_CHAT_ID.into(),
        sender_name: "Bob".into(),
        text: "threaded message".into(),
        timestamp: 1700000001,
        thread_id: Some("42".into()),
        meta: serde_json::json!({
            "chat_id": TEST_CHAT_ID,
            "message_id": 99,
        }),
    })])
    .await;

    let msgs = h.read_inbound();
    assert_eq!(msgs.len(), 1);

    let msg = &msgs[0];
    assert_eq!(msg.channel, TEST_CHANNEL);
    assert_eq!(msg.group, TEST_GROUP_SLUG);
    assert_eq!(msg.thread_id, Some("42".into()));
    assert_eq!(msg.sender, "Bob");
    assert_eq!(msg.text, "threaded message");
    assert_eq!(msg.timestamp, 1700000001);
}

/// Outbound nq files are validated before sending: malformed JSON and
/// channel-mismatched files are deleted, valid files are retained for delivery
/// (retried on transient provider failures).
#[tokio::test]
async fn e2e_outbound_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let outbox_dir = tmp.path().join("outbox");
    std::fs::create_dir_all(&outbox_dir).unwrap();

    // Test 1: Malformed JSON → file gets deleted.
    let malformed_path = nq::enqueue(&outbox_dir, 1000, b"not valid json").unwrap();
    assert!(malformed_path.exists());

    // poll_outbound needs a teloxide::Bot, but we're testing the validation path
    // which runs before any provider send. We create a Bot with a dummy token —
    // it won't make network calls for malformed/mismatched messages.
    let bot = teloxide::Bot::new("0:fake-token-for-test");

    kk_connector::outbound::poll_outbound(outbox_dir.to_str().unwrap(), TEST_CHANNEL, &bot)
        .await
        .unwrap();

    // Malformed file should be deleted.
    assert!(
        !malformed_path.exists(),
        "malformed nq file should have been deleted"
    );

    // Test 2: Channel mismatch → file gets deleted.
    let wrong_channel = OutboundMessage {
        channel: "other-bot".into(),
        group: "some-group".into(),
        thread_id: None,
        text: "hello".into(),
        meta: serde_json::json!({"chat_id": TEST_CHAT_ID}),
    };
    let mismatch_path = nq::enqueue(
        &outbox_dir,
        1001,
        &serde_json::to_vec(&wrong_channel).unwrap(),
    )
    .unwrap();
    assert!(mismatch_path.exists());

    kk_connector::outbound::poll_outbound(outbox_dir.to_str().unwrap(), TEST_CHANNEL, &bot)
        .await
        .unwrap();

    assert!(
        !mismatch_path.exists(),
        "channel-mismatched nq file should have been deleted"
    );

    // Test 3: Valid message with correct channel → file is retained (send fails with
    // dummy token, so the retry logic keeps the file).
    let valid = OutboundMessage {
        channel: TEST_CHANNEL.into(),
        group: TEST_GROUP_SLUG.into(),
        thread_id: None,
        text: "response text".into(),
        meta: serde_json::json!({"chat_id": TEST_CHAT_ID}),
    };
    let valid_path = nq::enqueue(&outbox_dir, 1002, &serde_json::to_vec(&valid).unwrap()).unwrap();
    assert!(valid_path.exists());

    kk_connector::outbound::poll_outbound(outbox_dir.to_str().unwrap(), TEST_CHANNEL, &bot)
        .await
        .unwrap();

    // The file should still exist because the send fails (dummy token),
    // and the retry logic retains the file.
    assert!(
        valid_path.exists(),
        "valid nq file should be retained when send fails"
    );
}

/// Message from an unmapped chat_id → auto-registers as `tg-{chat_id}`, persists
/// mapping to `groups.d/{channel}.json`, and enqueues the message with the new slug.
#[tokio::test]
async fn e2e_auto_register() {
    let mut h = TestHarness::empty();
    let unmapped_chat_id = "-1009999999999";

    h.process(vec![ConnectorEvent::Message(InboundRaw {
        chat_id: unmapped_chat_id.into(),
        sender_name: "Carol".into(),
        text: "@bot hello from new group".into(),
        timestamp: 1700000002,
        thread_id: None,
        meta: serde_json::json!({
            "chat_id": unmapped_chat_id,
            "message_id": 1,
        }),
    })])
    .await;

    // Verify: message was processed (not dropped)
    let msgs = h.read_inbound();
    assert_eq!(msgs.len(), 1, "auto-registered message should be enqueued");
    assert_eq!(msgs[0].group, "tg-1009999999999");
    assert_eq!(msgs[0].sender, "Carol");
    assert_eq!(msgs[0].text, "@bot hello from new group");

    // Verify: groups.d file was created with correct mapping
    assert!(h.groups_d_file.exists(), "groups.d file should be created");
    let gc = h.read_groups_config();
    assert!(gc.groups.contains_key("tg-1009999999999"));
    assert_eq!(
        gc.groups["tg-1009999999999"].channels[TEST_CHANNEL].chat_id,
        unmapped_chat_id
    );
}

/// Bot added to a new group (NewChat event) → auto-registers the chat in
/// `groups.d/{channel}.json`. No inbound nq file is created (registration only).
#[tokio::test]
async fn e2e_new_chat_event() {
    let mut h = TestHarness::empty();
    let new_chat_id = "-1008888888888";

    h.process(vec![ConnectorEvent::NewChat {
        chat_id: new_chat_id.into(),
        chat_title: Some("My New Group".into()),
    }])
    .await;

    // Verify: no inbound messages enqueued (NewChat is just registration)
    assert_eq!(
        h.read_inbound().len(),
        0,
        "NewChat should not enqueue messages"
    );

    // Verify: groups.d file was created with correct mapping
    assert!(h.groups_d_file.exists(), "groups.d file should be created");
    let gc = h.read_groups_config();
    assert!(gc.groups.contains_key("tg-1008888888888"));
    assert_eq!(
        gc.groups["tg-1008888888888"].channels[TEST_CHANNEL].chat_id,
        new_chat_id
    );
}

/// Two messages from the same unmapped chat_id → first triggers auto-registration,
/// second resolves from in-memory map. Both nq files have the same `tg-{id}` slug.
#[tokio::test]
async fn e2e_auto_register_reuses_slug() {
    let mut h = TestHarness::empty();
    let unmapped_chat_id = "-1007777777777";

    h.process(vec![
        ConnectorEvent::Message(InboundRaw {
            chat_id: unmapped_chat_id.into(),
            sender_name: "Dave".into(),
            text: "first message".into(),
            timestamp: 1700000010,
            thread_id: None,
            meta: serde_json::json!({ "chat_id": unmapped_chat_id, "message_id": 1 }),
        }),
        ConnectorEvent::Message(InboundRaw {
            chat_id: unmapped_chat_id.into(),
            sender_name: "Dave".into(),
            text: "second message".into(),
            timestamp: 1700000011,
            thread_id: None,
            meta: serde_json::json!({ "chat_id": unmapped_chat_id, "message_id": 2 }),
        }),
    ])
    .await;

    let msgs = h.read_inbound();
    assert_eq!(msgs.len(), 2, "both messages should be enqueued");
    assert_eq!(msgs[0].group, "tg-1007777777777");
    assert_eq!(msgs[1].group, "tg-1007777777777");
    assert_eq!(msgs[0].text, "first message");
    assert_eq!(msgs[1].text, "second message");
}
