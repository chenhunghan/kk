//! Local process launcher: spawns kk-agent as child processes.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::info;

use crate::config::GatewayConfig;
use kk_core::types::InboundMessage;

use super::{LaunchedAgent, Launcher, now_secs};

/// A locally spawned agent child process.
#[allow(dead_code)]
struct LocalChild {
    handle: tokio::process::Child,
    session_id: String,
    group: String,
    thread_id: Option<String>,
    created_at: u64,
}

pub struct LocalLauncher {
    agent_bin: String,
    data_dir: String,
    children: Arc<RwLock<HashMap<String, LocalChild>>>,
}

impl LocalLauncher {
    pub fn new(agent_bin: &str, data_dir: &str) -> Self {
        Self {
            agent_bin: agent_bin.to_string(),
            data_dir: data_dir.to_string(),
            children: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl Launcher for LocalLauncher {
    async fn launch(
        &self,
        job_name: &str,
        session_id: &str,
        msg: &InboundMessage,
        config: &GatewayConfig,
    ) -> Result<LaunchedAgent> {
        let mut cmd = Command::new(&self.agent_bin);
        cmd.env("SESSION_ID", session_id)
            .env("GROUP", &msg.group)
            .env("DATA_DIR", &self.data_dir)
            .env("IDLE_TIMEOUT", config.job_idle_timeout.to_string())
            .env("MAX_TURNS", config.job_max_turns.to_string());

        if let Some(ref tid) = msg.thread_id {
            cmd.env("THREAD_ID", tid);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn agent binary: {}", self.agent_bin))?;

        let now = now_secs();
        let handle_name = job_name.to_string();

        info!(
            handle = handle_name,
            pid = ?child.id(),
            session_id,
            "spawned local agent process"
        );

        self.children.write().await.insert(
            handle_name.clone(),
            LocalChild {
                handle: child,
                session_id: session_id.to_string(),
                group: msg.group.clone(),
                thread_id: msg.thread_id.clone(),
                created_at: now,
            },
        );

        Ok(LaunchedAgent {
            handle: handle_name,
            session_id: session_id.to_string(),
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: now,
        })
    }

    async fn list_active_handles(&self) -> Result<Vec<String>> {
        let mut children = self.children.write().await;
        let mut active = Vec::new();
        let mut finished = Vec::new();

        for (name, child) in children.iter_mut() {
            match child.handle.try_wait() {
                Ok(Some(_status)) => finished.push(name.clone()),
                Ok(None) => active.push(name.clone()),
                Err(_) => finished.push(name.clone()),
            }
        }

        for name in finished {
            children.remove(&name);
        }

        Ok(active)
    }

    async fn kill(&self, handle: &str) -> Result<()> {
        let mut children = self.children.write().await;
        if let Some(mut child) = children.remove(handle) {
            child.handle.kill().await.ok();
        }
        Ok(())
    }
}
