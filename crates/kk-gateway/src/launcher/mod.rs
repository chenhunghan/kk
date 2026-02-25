//! Agent launcher abstraction: K8s Jobs or local child processes.

#[cfg(feature = "k8s")]
pub mod k8s;
pub mod local;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::GatewayConfig;
use kk_core::types::InboundMessage;

/// Identifies a launched agent process/job.
#[derive(Debug, Clone)]
pub struct LaunchedAgent {
    /// Opaque handle (K8s Job name or local PID-based name).
    pub handle: String,
    pub session_id: String,
    pub group: String,
    pub thread_id: Option<String>,
    pub created_at: u64,
}

/// Abstracts how agent jobs are launched and managed.
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Launch a new agent for the given session.
    async fn launch(
        &self,
        job_name: &str,
        session_id: &str,
        msg: &InboundMessage,
        config: &GatewayConfig,
    ) -> Result<LaunchedAgent>;

    /// Return handles of all currently active agents.
    async fn list_active_handles(&self) -> Result<Vec<String>>;

    /// Stop/delete an agent by handle.
    async fn kill(&self, handle: &str) -> Result<()>;

    /// Clean up finished/expired agents. K8s deletes completed Jobs past TTL.
    /// Local mode is a no-op (processes self-exit).
    async fn cleanup_finished(&self, _ttl: u64) -> Result<()> {
        Ok(())
    }

    /// Recover active agents from the launcher's backend on startup.
    /// K8s scans Job labels; local returns empty (file-based recovery handles it).
    async fn recover(&self) -> Result<Vec<LaunchedAgent>> {
        Ok(Vec::new())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
