use anyhow::Result;
use kube::Client;
use tracing::info;

use kk_gateway::config::GatewayConfig;
use kk_gateway::loops;
use kk_gateway::state::SharedState;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();
    info!("starting kk-gateway");

    let config = GatewayConfig::from_env();
    let client = Client::try_default().await?;
    let paths = kk_core::paths::DataPaths::new(&config.data_dir);
    paths.ensure_dirs()?;

    let state = SharedState::new(config.clone(), client.clone(), &paths)?;

    // Rebuild activeJobs from existing K8s Jobs (handles gateway restarts)
    if let Err(e) = state.rebuild_active_jobs().await {
        tracing::warn!(error = %e, "failed to rebuild activeJobs from K8s (will start fresh)");
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
