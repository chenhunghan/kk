use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};

use kk_core::paths::DataPaths;
use kk_core::types::{GroupsConfig, RequestManifest};

use crate::config::GatewayConfig;
use crate::launcher::Launcher;

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
    pub launcher: Arc<dyn Launcher>,
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
    pub fn new(
        config: GatewayConfig,
        launcher: Arc<dyn Launcher>,
        paths: &DataPaths,
    ) -> Result<Self> {
        let groups_config = load_groups_config(paths)?;
        info!(groups = groups_config.groups.len(), "loaded groups config");

        Ok(Self {
            paths: paths.clone(),
            config,
            launcher,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            groups_config: Arc::new(RwLock::new(groups_config)),
            stream_offsets: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// File-based recovery: scan /data/results/*/status for "running" entries.
    /// Works identically in K8s and local mode.
    pub async fn rebuild_active_jobs_from_files(&self) -> Result<()> {
        let results_base = self.paths.results_dir("");
        let parent = results_base.parent().unwrap_or(&results_base);

        if !parent.exists() {
            return Ok(());
        }

        let entries = match std::fs::read_dir(parent) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };

        let mut active = self.active_jobs.write().await;
        let mut count = 0u32;

        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name.starts_with('.') || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            {
                continue;
            }

            let status_path = self.paths.result_status(&dir_name);
            let status = match std::fs::read_to_string(&status_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if status.trim() != "running" {
                continue;
            }

            // Read request.json to recover group/thread_id
            let manifest_path = self.paths.request_manifest(&dir_name);
            let manifest: RequestManifest = match std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(m) => m,
                None => continue,
            };

            let key = routing_key(&manifest.group, manifest.thread_id.as_deref());
            info!(
                session_id = dir_name,
                group = manifest.group,
                routing_key = key,
                "recovered active job from status file"
            );
            active.insert(
                key,
                ActiveJob {
                    job_name: format!("recovered-{dir_name}"),
                    session_id: dir_name,
                    group: manifest.group,
                    thread_id: manifest.thread_id,
                    created_at: 0,
                },
            );
            count += 1;
        }

        if count > 0 {
            info!(count, "rebuilt active_jobs from status files");
        }
        Ok(())
    }

    /// Launcher-specific recovery (K8s recovers from Job labels, local is no-op).
    pub async fn rebuild_active_jobs_from_launcher(&self) -> Result<()> {
        let recovered = self.launcher.recover().await?;
        if recovered.is_empty() {
            return Ok(());
        }

        let mut active = self.active_jobs.write().await;
        for agent in recovered {
            let key = routing_key(&agent.group, agent.thread_id.as_deref());
            // Only insert if not already recovered from files
            active.entry(key.clone()).or_insert_with(|| {
                info!(
                    job = agent.handle,
                    session_id = agent.session_id,
                    routing_key = key,
                    "recovered active job from launcher"
                );
                ActiveJob {
                    job_name: agent.handle,
                    session_id: agent.session_id,
                    group: agent.group,
                    thread_id: agent.thread_id,
                    created_at: agent.created_at,
                }
            });
        }

        info!(count = active.len(), "rebuilt active_jobs from launcher");
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
