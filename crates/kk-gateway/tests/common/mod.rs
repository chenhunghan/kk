#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use kk_core::nq;
use kk_core::paths::DataPaths;
use kk_core::types::{
    ChannelMapping, ChannelType, FollowUpMessage, GroupEntry, GroupsConfig, InboundMessage,
    OutboundMessage, RequestManifest, TriggerMode,
};
use kk_gateway::config::GatewayConfig;
use kk_gateway::launcher::{LaunchedAgent, Launcher};
use kk_gateway::state::{ActiveJob, SharedState};
use tokio::sync::RwLock;

/// A no-op launcher for tests that don't actually launch agents.
pub struct MockLauncher {
    pub launched: Arc<RwLock<Vec<LaunchedAgent>>>,
}

impl MockLauncher {
    pub fn new() -> Self {
        Self {
            launched: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl Launcher for MockLauncher {
    async fn launch(
        &self,
        job_name: &str,
        session_id: &str,
        msg: &InboundMessage,
        _config: &GatewayConfig,
    ) -> anyhow::Result<LaunchedAgent> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let agent = LaunchedAgent {
            handle: job_name.to_string(),
            session_id: session_id.to_string(),
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: now,
        };
        self.launched.write().await.push(agent.clone());
        Ok(agent)
    }

    async fn list_active_handles(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .launched
            .read()
            .await
            .iter()
            .map(|a| a.handle.clone())
            .collect())
    }

    async fn kill(&self, handle: &str) -> anyhow::Result<()> {
        let mut launched = self.launched.write().await;
        launched.retain(|a| a.handle != handle);
        Ok(())
    }
}

/// Test harness for gateway e2e tests.
/// Creates a temp directory with the full PVC structure and a SharedState
/// with a mock launcher (never calls K8s).
pub struct GatewayTestHarness {
    pub state: SharedState,
    pub paths: DataPaths,
    pub _tmp: tempfile::TempDir,
}

impl GatewayTestHarness {
    /// Create a new harness with empty groups config.
    pub async fn new() -> Self {
        Self::with_groups(GroupsConfig {
            groups: HashMap::new(),
        })
        .await
    }

    /// Create a new harness with pre-populated groups config.
    pub async fn with_groups(groups: GroupsConfig) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::new(tmp.path());
        paths.ensure_dirs().unwrap();

        // Write groups config so SharedState::new can load it
        let groups_json = serde_json::to_string_pretty(&groups).unwrap();
        std::fs::write(paths.groups_json(), &groups_json).unwrap();

        let config = GatewayConfig {
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
        };

        let launcher: Arc<dyn Launcher> = Arc::new(MockLauncher::new());

        let state = SharedState {
            config,
            launcher,
            paths: paths.clone(),
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            groups_config: Arc::new(RwLock::new(groups)),
            stream_offsets: Arc::new(RwLock::new(HashMap::new())),
        };

        Self {
            state,
            paths,
            _tmp: tmp,
        }
    }

    /// Add/override a group in the groups config.
    pub async fn set_group(&self, slug: &str, entry: GroupEntry) {
        let mut groups = self.state.groups_config.write().await;
        groups.groups.insert(slug.to_string(), entry);
    }

    /// Enqueue an inbound message to /data/inbound/.
    pub fn enqueue_inbound(&self, msg: &InboundMessage) {
        let payload = serde_json::to_vec(msg).unwrap();
        nq::enqueue(&self.paths.inbound_dir(), msg.timestamp, &payload).unwrap();
    }

    /// Simulate cold_path file artifacts without actually launching an agent.
    /// Creates results dir + request.json + status=running + active_jobs entry.
    /// Returns the session_id.
    pub async fn simulate_cold_path(&self, msg: &InboundMessage) -> String {
        let session_id = kk_core::paths::session_id_threaded(
            &msg.group,
            msg.thread_id.as_deref(),
            msg.timestamp,
        );
        let job_name = kk_core::paths::job_name(&session_id);

        // Create results directory
        let results_dir = self.paths.results_dir(&session_id);
        std::fs::create_dir_all(&results_dir).unwrap();

        // Write request.json
        let manifest = RequestManifest {
            channel: msg.channel.clone(),
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            sender: msg.sender.clone(),
            meta: msg.meta.clone(),
            messages: vec![kk_core::types::RequestMessage {
                sender: msg.sender.clone(),
                text: msg.text.clone(),
                ts: msg.timestamp,
            }],
        };
        let manifest_path = self.paths.request_manifest(&session_id);
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Write status=running
        std::fs::write(self.paths.result_status(&session_id), "running").unwrap();

        // Ensure group queue dir
        let queue_dir = self
            .paths
            .group_queue_dir_threaded(&msg.group, msg.thread_id.as_deref());
        std::fs::create_dir_all(&queue_dir).unwrap();

        // Add to active_jobs
        let key = kk_gateway::state::routing_key(&msg.group, msg.thread_id.as_deref());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut active = self.state.active_jobs.write().await;
        active.insert(
            key,
            ActiveJob {
                job_name,
                session_id: session_id.clone(),
                group: msg.group.clone(),
                thread_id: msg.thread_id.clone(),
                created_at: now,
            },
        );

        session_id
    }

    /// Simulate agent completing with a response.
    pub fn simulate_agent_done(&self, session_id: &str, response_text: &str) {
        let response_path = self.paths.result_response(session_id);
        let mut f = std::fs::File::create(&response_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"result","result":{}}}"#,
            serde_json::to_string(response_text).unwrap()
        )
        .unwrap();

        // Set status=done
        std::fs::write(self.paths.result_status(session_id), "done").unwrap();
    }

    /// Simulate agent error.
    pub fn simulate_agent_error(&self, session_id: &str) {
        std::fs::write(self.paths.result_status(session_id), "error").unwrap();
    }

    /// Pre-populate an active job entry.
    pub async fn add_active_job(&self, routing_key: &str, job: ActiveJob) {
        let mut active = self.state.active_jobs.write().await;
        active.insert(routing_key.to_string(), job);
    }

    /// Read outbound messages from a channel's outbox.
    pub fn read_outbound(&self, channel: &str) -> Vec<OutboundMessage> {
        let outbox = self.paths.outbox_dir(channel);
        let files = match nq::list_pending(&outbox) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        files
            .iter()
            .filter_map(|f| {
                let data = nq::read_message(f).ok()?;
                serde_json::from_slice(&data).ok()
            })
            .collect()
    }

    /// Read follow-up messages from a group queue.
    pub fn read_follow_ups(&self, group: &str, thread_id: Option<&str>) -> Vec<FollowUpMessage> {
        let queue_dir = self.paths.group_queue_dir_threaded(group, thread_id);
        let files = match nq::list_pending(&queue_dir) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        files
            .iter()
            .filter_map(|f| {
                let data = nq::read_message(f).ok()?;
                serde_json::from_slice(&data).ok()
            })
            .collect()
    }

    /// Read a request manifest from results.
    pub fn read_manifest(&self, session_id: &str) -> RequestManifest {
        let path = self.paths.request_manifest(session_id);
        let content = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    /// Get count of active jobs.
    pub async fn active_job_count(&self) -> usize {
        self.state.active_jobs.read().await.len()
    }

    /// List archived session IDs in .done/.
    pub fn list_archived(&self) -> Vec<String> {
        let done_dir = self.paths.results_done_dir();
        if !done_dir.exists() {
            return Vec::new();
        }
        std::fs::read_dir(&done_dir)
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                if e.file_type().ok()?.is_dir() {
                    Some(e.file_name().to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Build a sample InboundMessage for testing.
pub fn sample_inbound(group: &str, text: &str) -> InboundMessage {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    InboundMessage {
        channel: "test-channel".to_string(),
        channel_type: ChannelType::Telegram,
        group: group.to_string(),
        thread_id: None,
        sender: "alice".to_string(),
        text: text.to_string(),
        timestamp: now,
        meta: serde_json::json!({}),
    }
}

/// Build a sample GroupEntry with trigger_mode=Always.
pub fn always_group() -> GroupEntry {
    GroupEntry {
        trigger_mode: TriggerMode::Always,
        trigger_pattern: None,
        channels: HashMap::from([(
            "test-channel".to_string(),
            ChannelMapping {
                chat_id: "-100".to_string(),
            },
        )]),
    }
}
