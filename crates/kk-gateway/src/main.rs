use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use kk_gateway::config::GatewayConfig;
use kk_gateway::launcher::Launcher;
use kk_gateway::launcher::local::LocalLauncher;
use kk_gateway::loops;
use kk_gateway::state::SharedState;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();
    info!("starting kk-gateway");

    let config = GatewayConfig::from_env();
    let paths = kk_core::paths::DataPaths::new(&config.data_dir);
    paths.ensure_dirs()?;

    let mode = std::env::var("KK_MODE").unwrap_or_else(|_| "k8s".into());

    let launcher: Arc<dyn Launcher> = match mode.as_str() {
        "local" => {
            let agent_bin = std::env::var("AGENT_BIN").unwrap_or_else(|_| "kk-agent".into());
            info!(agent_bin, "using local process launcher");
            Arc::new(LocalLauncher::new(&agent_bin, &config.data_dir))
        }
        _ => {
            #[cfg(feature = "k8s")]
            {
                let client = kube::Client::try_default().await?;
                info!("using K8s Job launcher");
                Arc::new(kk_gateway::launcher::k8s::K8sLauncher::new(
                    client,
                    &config.namespace,
                ))
            }
            #[cfg(not(feature = "k8s"))]
            {
                anyhow::bail!(
                    "K8s mode requires the 'k8s' feature flag. Use KK_MODE=local for local mode."
                );
            }
        }
    };

    let state = SharedState::new(config.clone(), launcher, &paths)?;

    // File-based recovery (works in both modes)
    if let Err(e) = state.rebuild_active_jobs_from_files().await {
        warn!(error = %e, "failed to rebuild activeJobs from status files");
    }

    // Launcher-specific recovery (K8s recovers from Job labels)
    if let Err(e) = state.rebuild_active_jobs_from_launcher().await {
        warn!(error = %e, "failed to rebuild activeJobs from launcher");
    }

    let shared = state.clone();
    let mut health = kk_core::health::HealthServer::new(8082);
    health.route("/status", move || {
        let s = shared.clone();
        async move { s.status_json().await }
    });

    let health_server = tokio::spawn(async move {
        health.run().await.unwrap();
    });

    info!("health server listening on :8082");

    let inbound = tokio::spawn(loops::inbound::run(state.clone()));
    let results = tokio::spawn(loops::results::run(state.clone()));
    let cleanup = tokio::spawn(loops::cleanup::run(state.clone()));
    let state_reload = tokio::spawn(loops::state_reload::run(state.clone()));

    tokio::select! {
        res = inbound => { res??; }
        res = results => { res??; }
        res = cleanup => { res??; }
        res = state_reload => { res??; }
        res = health_server => { res?; }
    }

    Ok(())
}
