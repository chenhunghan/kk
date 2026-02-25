use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::Job;
use kube::api::ListParams;
use kube::{Api, Client};
use tokio::sync::RwLock;
use tracing::{info, warn};

use kk_core::paths::DataPaths;
use kk_core::types::GroupsConfig;

use crate::config::GatewayConfig;

/// Tracks a running Agent Job for a group (or group+thread).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveJob {
    pub job_name: String,
    pub session_id: String,
    pub group: String,
    pub thread_id: Option<String>,
    pub created_at: u64,
}

/// Shared gateway state, behind an Arc for concurrent access.
#[derive(Clone)]
pub struct SharedState {
    pub config: GatewayConfig,
    pub client: Client,
    pub paths: DataPaths,
    pub active_jobs: Arc<RwLock<HashMap<String, ActiveJob>>>,
    pub groups_config: Arc<RwLock<GroupsConfig>>,
    /// Tracks the last byte offset read from response.jsonl per session_id (for streaming).
    pub stream_offsets: Arc<RwLock<HashMap<String, u64>>>,
}

/// Build a routing key for active_jobs lookup.
/// Thread-aware: `group` or `group|thread_id`.
pub fn routing_key(group: &str, thread_id: Option<&str>) -> String {
    match thread_id {
        Some(tid) => format!("{group}|{tid}"),
        None => group.to_string(),
    }
}

impl SharedState {
    pub fn new(config: GatewayConfig, client: Client, paths: &DataPaths) -> Result<Self> {
        let groups_config = load_groups_config(paths)?;
        info!(groups = groups_config.groups.len(), "loaded groups config");

        Ok(Self {
            paths: paths.clone(),
            config,
            client,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            groups_config: Arc::new(RwLock::new(groups_config)),
            stream_offsets: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Rebuild active_jobs from existing K8s Jobs on startup.
    /// This handles gateway restarts — we scan for running agent Jobs
    /// so we don't create duplicates.
    pub async fn rebuild_active_jobs(&self) -> Result<()> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let lp = ListParams::default().labels("app=kk-agent");
        let job_list = jobs_api
            .list(&lp)
            .await
            .context("failed to list agent Jobs")?;

        let mut active = self.active_jobs.write().await;
        for job in job_list.items {
            let labels = job.metadata.labels.as_ref();
            let name = job.metadata.name.clone().unwrap_or_default();

            // Skip completed/failed Jobs
            if let Some(status) = &job.status
                && (status.succeeded.unwrap_or(0) > 0 || status.failed.unwrap_or(0) > 0)
            {
                continue;
            }

            let group = labels
                .and_then(|l| l.get("kk.io/group"))
                .cloned()
                .unwrap_or_default();
            let session_id = labels
                .and_then(|l| l.get("kk.io/session-id"))
                .cloned()
                .unwrap_or_default();
            let thread_id = labels.and_then(|l| l.get("kk.io/thread-id")).cloned();

            if group.is_empty() || session_id.is_empty() {
                warn!(job = name, "agent Job missing required labels, skipping");
                continue;
            }

            let created_at = job
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.timestamp() as u64)
                .unwrap_or(0);

            let key = routing_key(&group, thread_id.as_deref());
            info!(job = name, group, routing_key = key, "recovered active Job");
            active.insert(
                key,
                ActiveJob {
                    job_name: name,
                    session_id,
                    group,
                    thread_id,
                    created_at,
                },
            );
        }

        info!(count = active.len(), "rebuilt active_jobs from K8s");
        Ok(())
    }

    /// Reload groups config from disk (groups.json + groups.d/ merge).
    pub async fn reload_groups_config(&self) -> Result<()> {
        let config = load_groups_config(&self.paths)?;
        let mut groups = self.groups_config.write().await;
        *groups = config;
        Ok(())
    }

    pub async fn status_json(&self) -> String {
        let active_jobs = self.active_jobs.read().await;
        let groups = self.groups_config.read().await;
        serde_json::json!({
            "active_jobs": active_jobs.len(),
            "group_count": groups.groups.len(),
        })
        .to_string()
    }
}

/// Load groups config by merging groups.d/*.json files with groups.json overlay.
/// groups.d/ = auto-registered by connectors (baseline).
/// groups.json = admin overrides (takes precedence).
fn load_groups_config(paths: &DataPaths) -> Result<GroupsConfig> {
    let mut merged = GroupsConfig {
        groups: HashMap::new(),
    };

    // First, load all groups.d/*.json files (auto-registered)
    let groups_d = paths.groups_d_dir();
    if groups_d.exists()
        && let Ok(entries) = std::fs::read_dir(&groups_d)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<GroupsConfig>(&content) {
                    Ok(partial) => {
                        for (k, v) in partial.groups {
                            merged.groups.insert(k, v);
                        }
                    }
                    Err(e) => {
                        warn!(file = %path.display(), error = %e, "invalid groups.d file, skipping");
                    }
                },
                Err(e) => {
                    warn!(file = %path.display(), error = %e, "failed to read groups.d file");
                }
            }
        }
    }

    // Then overlay groups.json (admin overrides take precedence)
    match std::fs::read_to_string(paths.groups_json()) {
        Ok(content) => match serde_json::from_str::<GroupsConfig>(&content) {
            Ok(admin) => {
                for (k, v) in admin.groups {
                    merged.groups.insert(k, v);
                }
            }
            Err(e) => {
                warn!(error = %e, "invalid groups.json, using groups.d only");
            }
        },
        Err(_) => {
            // No groups.json is fine — only groups.d entries
        }
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_routing_key() {
        assert_eq!(routing_key("family-chat", None), "family-chat");
        assert_eq!(routing_key("family-chat", Some("42")), "family-chat|42");
        assert_eq!(
            routing_key("eng", Some("1708801285.000050")),
            "eng|1708801285.000050"
        );
    }

    #[test]
    fn test_load_groups_config_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::new(dir.path());
        paths.ensure_dirs().unwrap();

        let config = load_groups_config(&paths).unwrap();
        assert!(config.groups.is_empty());
    }

    #[test]
    fn test_load_groups_config_merge() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::new(dir.path());
        paths.ensure_dirs().unwrap();

        // Write a groups.d file (auto-registered by connector)
        let groups_d_file = paths.groups_d_dir().join("telegram-bot.json");
        std::fs::write(
            &groups_d_file,
            r#"{"groups":{"family":{"trigger_mode":"always","channels":{"tg":{"chat_id":"-100"}}}}}"#,
        )
        .unwrap();

        // Write groups.json (admin override — overrides family, adds work)
        std::fs::write(
            paths.groups_json(),
            r#"{"groups":{"family":{"trigger_mode":"mention","trigger_pattern":"@bot","channels":{"tg":{"chat_id":"-100"}}},"work":{"trigger_mode":"prefix","trigger_pattern":"/ask","channels":{"slack":{"chat_id":"C123"}}}}}"#,
        )
        .unwrap();

        let config = load_groups_config(&paths).unwrap();
        assert_eq!(config.groups.len(), 2);
        // Admin override should win for "family"
        assert_eq!(
            config.groups["family"].trigger_mode,
            kk_core::types::TriggerMode::Mention
        );
        assert!(config.groups.contains_key("work"));
    }
}
