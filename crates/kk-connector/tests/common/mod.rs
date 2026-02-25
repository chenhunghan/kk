//! Shared test harness and parameterized test macro for all providers.
//!
//! Each provider test file (e2e_telegram.rs, e2e_slack.rs, etc.) invokes the
//! `provider_e2e_tests!` macro with provider-specific config. When adding a new
//! provider, create a new `e2e_{provider}.rs` — no changes to existing files.

use std::path::PathBuf;

use tokio::sync::mpsc;

use kk_connector::config::ConnectorConfig;
use kk_connector::groups::GroupMap;
use kk_connector::inbound::process_inbound;
use kk_connector::provider::ConnectorEvent;
use kk_core::nq;
use kk_core::types::{ChannelType, GroupsConfig, InboundMessage};

pub const TEST_GROUP_SLUG: &str = "dev-team";
pub const TEST_CHANNEL: &str = "test-bot-channel";

/// Provider-specific parameters for the shared test suite.
pub struct ProviderTestConfig {
    pub channel_type: &'static str,
    pub chat_id: &'static str,
    pub expected_channel_type: ChannelType,
    pub expected_auto_slug: &'static str,
    pub meta_chat_id_key: &'static str,
}

/// Common test scaffold: temp dirs, config, and group map.
pub struct TestHarness {
    pub config: ConnectorConfig,
    pub group_map: GroupMap,
    pub inbound_dir: PathBuf,
    pub groups_d_file: PathBuf,
    _tmp: tempfile::TempDir,
}

impl TestHarness {
    pub fn empty(provider: &ProviderTestConfig) -> Self {
        Self::new(provider, None)
    }

    pub fn with_mapping(provider: &ProviderTestConfig, chat_id: &str, slug: &str) -> Self {
        Self::new(provider, Some((chat_id, slug)))
    }

    fn new(provider: &ProviderTestConfig, mapping: Option<(&str, &str)>) -> Self {
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
            channel_type: provider.channel_type.into(),
            channel_name: TEST_CHANNEL.into(),
            inbound_dir: inbound_dir.to_str().unwrap().into(),
            outbox_dir: tmp.path().join("outbox").to_str().unwrap().into(),
            stream_dir: tmp.path().join("stream").to_str().unwrap().into(),
            groups_d_file: groups_d_file.to_str().unwrap().into(),
            telegram_bot_token: None,
            slack_bot_token: None,
            slack_app_token: None,
            discord_bot_token: None,
            github_token: None,
            github_webhook_secret: None,
            github_webhook_port: 8084,
            whatsapp_token: None,
            whatsapp_phone_number_id: None,
            whatsapp_webhook_verify_token: None,
            whatsapp_webhook_port: 8085,
            teams_app_id: None,
            teams_app_password: None,
            teams_webhook_port: 8086,
            gchat_token: None,
            gchat_webhook_port: 8087,
            linear_api_key: None,
            linear_webhook_port: 8088,
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
    pub async fn process(&mut self, events: Vec<ConnectorEvent>) {
        let (tx, rx) = mpsc::channel::<ConnectorEvent>(16);
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        let gm = self.group_map.clone();
        process_inbound(rx, &self.config, gm).await;
        self.group_map = GroupMap::load(&self.config.groups_d_file, &self.config.channel_name);
    }

    /// Read all pending inbound nq messages.
    pub fn read_inbound(&self) -> Vec<InboundMessage> {
        nq::list_pending(&self.inbound_dir)
            .unwrap()
            .iter()
            .map(|p| serde_json::from_slice(&nq::read_message(p).unwrap()).unwrap())
            .collect()
    }

    /// Read the persisted groups.d config file.
    pub fn read_groups_config(&self) -> GroupsConfig {
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

/// Build inbound meta with the provider-specific chat ID key.
pub fn make_inbound_meta(meta_key: &str, chat_id: &str, message_id: i64) -> serde_json::Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        meta_key.to_string(),
        serde_json::Value::String(chat_id.to_string()),
    );
    meta.insert("message_id".to_string(), serde_json::json!(message_id));
    serde_json::Value::Object(meta)
}

/// Build outbound meta with the provider-specific chat ID key.
pub fn make_outbound_meta(meta_key: &str, chat_id: &str) -> serde_json::Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        meta_key.to_string(),
        serde_json::Value::String(chat_id.to_string()),
    );
    serde_json::Value::Object(meta)
}

/// Generates the standard e2e test suite for a provider.
///
/// Usage (in `tests/e2e_{provider}.rs`):
/// ```ignore
/// mod common;
/// common::provider_e2e_tests! {
///     channel_type: "telegram",
///     chat_id: "-1001234567890",
///     expected_channel_type: kk_core::types::ChannelType::Telegram,
///     expected_auto_slug: "tg-1001234567890",
///     meta_chat_id_key: "chat_id",
///     dummy_sender: || { TelegramOutbound::new(teloxide::Bot::new("0:fake")) },
/// }
/// ```
macro_rules! provider_e2e_tests {
    (
        channel_type: $channel_type:expr,
        chat_id: $chat_id:expr,
        expected_channel_type: $ct_enum:expr,
        expected_auto_slug: $expected_slug:expr,
        meta_chat_id_key: $meta_key:expr,
        dummy_sender: $sender_fn:expr $(,)?
    ) => {
        use common::{
            ProviderTestConfig, TEST_CHANNEL, TEST_GROUP_SLUG, TestHarness, make_inbound_meta,
            make_outbound_meta,
        };
        use kk_connector::provider::{ConnectorEvent, InboundRaw};
        use kk_core::nq;
        use kk_core::types::OutboundMessage;

        fn cfg() -> ProviderTestConfig {
            ProviderTestConfig {
                channel_type: $channel_type,
                chat_id: $chat_id,
                expected_channel_type: $ct_enum,
                expected_auto_slug: $expected_slug,
                meta_chat_id_key: $meta_key,
            }
        }

        /// Message in a known group → nq file written with correct fields.
        #[tokio::test]
        async fn inbound_lifecycle() {
            let cfg = cfg();
            let mut h = TestHarness::with_mapping(&cfg, cfg.chat_id, TEST_GROUP_SLUG);

            h.process(vec![ConnectorEvent::Message(InboundRaw {
                chat_id: cfg.chat_id.into(),
                sender_name: "Alice".into(),
                text: "hello world".into(),
                timestamp: 1700000000,
                thread_id: None,
                meta: make_inbound_meta(cfg.meta_chat_id_key, cfg.chat_id, 42),
            })])
            .await;

            let msgs = h.read_inbound();
            assert_eq!(msgs.len(), 1, "expected exactly one nq file");

            let msg = &msgs[0];
            assert_eq!(msg.channel, TEST_CHANNEL);
            assert_eq!(msg.channel_type, cfg.expected_channel_type);
            assert_eq!(msg.group, TEST_GROUP_SLUG);
            assert_eq!(msg.thread_id, None);
            assert_eq!(msg.sender, "Alice");
            assert_eq!(msg.text, "hello world");
            assert_eq!(msg.timestamp, 1700000000);
            assert_eq!(msg.meta[cfg.meta_chat_id_key], cfg.chat_id);
            assert_eq!(msg.meta["message_id"], 42);
        }

        /// Threaded message → nq file has `thread_id` set.
        #[tokio::test]
        async fn inbound_threaded() {
            let cfg = cfg();
            let mut h = TestHarness::with_mapping(&cfg, cfg.chat_id, TEST_GROUP_SLUG);

            h.process(vec![ConnectorEvent::Message(InboundRaw {
                chat_id: cfg.chat_id.into(),
                sender_name: "Bob".into(),
                text: "threaded message".into(),
                timestamp: 1700000001,
                thread_id: Some("42".into()),
                meta: make_inbound_meta(cfg.meta_chat_id_key, cfg.chat_id, 99),
            })])
            .await;

            let msgs = h.read_inbound();
            assert_eq!(msgs.len(), 1);

            let msg = &msgs[0];
            assert_eq!(msg.channel, TEST_CHANNEL);
            assert_eq!(msg.channel_type, cfg.expected_channel_type);
            assert_eq!(msg.group, TEST_GROUP_SLUG);
            assert_eq!(msg.thread_id, Some("42".into()));
            assert_eq!(msg.sender, "Bob");
            assert_eq!(msg.text, "threaded message");
        }

        /// Unmapped chat_id → auto-registers, persists groups.d, enqueues message.
        #[tokio::test]
        async fn auto_register() {
            let cfg = cfg();
            let mut h = TestHarness::empty(&cfg);

            h.process(vec![ConnectorEvent::Message(InboundRaw {
                chat_id: cfg.chat_id.into(),
                sender_name: "Carol".into(),
                text: "@bot hello from new group".into(),
                timestamp: 1700000002,
                thread_id: None,
                meta: make_inbound_meta(cfg.meta_chat_id_key, cfg.chat_id, 1),
            })])
            .await;

            let msgs = h.read_inbound();
            assert_eq!(msgs.len(), 1, "auto-registered message should be enqueued");
            assert_eq!(msgs[0].group, cfg.expected_auto_slug);
            assert_eq!(msgs[0].channel_type, cfg.expected_channel_type);
            assert_eq!(msgs[0].sender, "Carol");

            assert!(h.groups_d_file.exists(), "groups.d file should be created");
            let gc = h.read_groups_config();
            assert!(gc.groups.contains_key(cfg.expected_auto_slug));
            assert_eq!(
                gc.groups[cfg.expected_auto_slug].channels[TEST_CHANNEL].chat_id,
                cfg.chat_id
            );
        }

        /// NewChat event → groups.d mapping created, no inbound nq file.
        #[tokio::test]
        async fn new_chat_event() {
            let cfg = cfg();
            let mut h = TestHarness::empty(&cfg);

            h.process(vec![ConnectorEvent::NewChat {
                chat_id: cfg.chat_id.into(),
                chat_title: Some("My New Group".into()),
            }])
            .await;

            assert_eq!(
                h.read_inbound().len(),
                0,
                "NewChat should not enqueue messages"
            );
            assert!(h.groups_d_file.exists(), "groups.d file should be created");
            let gc = h.read_groups_config();
            assert!(gc.groups.contains_key(cfg.expected_auto_slug));
            assert_eq!(
                gc.groups[cfg.expected_auto_slug].channels[TEST_CHANNEL].chat_id,
                cfg.chat_id
            );
        }

        /// Two messages from same unmapped chat_id → both get the same auto slug.
        #[tokio::test]
        async fn auto_register_reuses_slug() {
            let cfg = cfg();
            let mut h = TestHarness::empty(&cfg);

            h.process(vec![
                ConnectorEvent::Message(InboundRaw {
                    chat_id: cfg.chat_id.into(),
                    sender_name: "Dave".into(),
                    text: "first message".into(),
                    timestamp: 1700000010,
                    thread_id: None,
                    meta: make_inbound_meta(cfg.meta_chat_id_key, cfg.chat_id, 1),
                }),
                ConnectorEvent::Message(InboundRaw {
                    chat_id: cfg.chat_id.into(),
                    sender_name: "Dave".into(),
                    text: "second message".into(),
                    timestamp: 1700000011,
                    thread_id: None,
                    meta: make_inbound_meta(cfg.meta_chat_id_key, cfg.chat_id, 2),
                }),
            ])
            .await;

            let msgs = h.read_inbound();
            assert_eq!(msgs.len(), 2, "both messages should be enqueued");
            assert_eq!(msgs[0].group, cfg.expected_auto_slug);
            assert_eq!(msgs[1].group, cfg.expected_auto_slug);
            assert_eq!(msgs[0].text, "first message");
            assert_eq!(msgs[1].text, "second message");
        }

        /// Outbound validation: malformed/mismatched files deleted, valid retained.
        #[tokio::test]
        async fn outbound_validation() {
            let cfg = cfg();
            let tmp = tempfile::tempdir().unwrap();
            let outbox_dir = tmp.path().join("outbox");
            std::fs::create_dir_all(&outbox_dir).unwrap();

            let sender = ($sender_fn)();

            // Test 1: Malformed JSON → file gets deleted.
            let malformed_path = nq::enqueue(&outbox_dir, 1000, b"not valid json").unwrap();
            assert!(malformed_path.exists());

            kk_connector::outbound::poll_outbound(
                outbox_dir.to_str().unwrap(),
                TEST_CHANNEL,
                &sender,
            )
            .await
            .unwrap();

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
                meta: make_outbound_meta(cfg.meta_chat_id_key, cfg.chat_id),
            };
            let mismatch_path = nq::enqueue(
                &outbox_dir,
                1001,
                &serde_json::to_vec(&wrong_channel).unwrap(),
            )
            .unwrap();
            assert!(mismatch_path.exists());

            kk_connector::outbound::poll_outbound(
                outbox_dir.to_str().unwrap(),
                TEST_CHANNEL,
                &sender,
            )
            .await
            .unwrap();

            assert!(
                !mismatch_path.exists(),
                "channel-mismatched nq file should have been deleted"
            );

            // Test 3: Valid message with correct channel → file is retained
            // (send fails with dummy credentials, so retry logic keeps the file).
            let valid = OutboundMessage {
                channel: TEST_CHANNEL.into(),
                group: TEST_GROUP_SLUG.into(),
                thread_id: None,
                text: "response text".into(),
                meta: make_outbound_meta(cfg.meta_chat_id_key, cfg.chat_id),
            };
            let valid_path =
                nq::enqueue(&outbox_dir, 1002, &serde_json::to_vec(&valid).unwrap()).unwrap();
            assert!(valid_path.exists());

            kk_connector::outbound::poll_outbound(
                outbox_dir.to_str().unwrap(),
                TEST_CHANNEL,
                &sender,
            )
            .await
            .unwrap();

            assert!(
                valid_path.exists(),
                "valid nq file should be retained when send fails"
            );
        }
    };
}
pub(crate) use provider_e2e_tests;
