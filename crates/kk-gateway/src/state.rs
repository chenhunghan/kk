use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use kube::Client;
use tokio::sync::RwLock;
use tracing::info;

use kk_core::paths::DataPaths;
use kk_core::types::GroupsConfig;

use crate::config::GatewayConfig;

/// Tracks a running Agent Job for a group.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveJob {
    pub job_name: String,
    pub session_id: String,
    pub created_at: u64,
}

/// Shared gateway state, behind an Arc for concurrent access.
#[derive(Clone)]
#[allow(dead_code)]
pub struct SharedState {
    pub config: GatewayConfig,
    pub client: Client,
    pub paths: DataPaths,
    pub active_jobs: Arc<RwLock<HashMap<String, ActiveJob>>>,
    pub groups_config: Arc<RwLock<GroupsConfig>>,
}

impl SharedState {
    pub fn new(config: GatewayConfig, client: Client, paths: &DataPaths) -> Result<Self> {
        // Load groups.json if it exists
        let groups_config = match std::fs::read_to_string(paths.groups_json()) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| GroupsConfig {
                groups: HashMap::new(),
            }),
            Err(_) => GroupsConfig {
                groups: HashMap::new(),
            },
        };

        info!(groups = groups_config.groups.len(), "loaded groups.json");

        Ok(Self {
            paths: paths.clone(),
            config,
            client,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            groups_config: Arc::new(RwLock::new(groups_config)),
        })
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
