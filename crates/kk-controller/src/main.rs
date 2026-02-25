use anyhow::Result;
use axum::{Router, routing::get};
use kk_controller::{config::ControllerConfig, reconcilers};
use kk_core::paths::DataPaths;
use kube::Client;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();
    info!("starting kk-controller");

    let config = ControllerConfig::from_env();
    info!(
        namespace = config.namespace.as_str(),
        data_dir = config.data_dir.as_str(),
        "loaded config"
    );

    // Ensure PVC directory structure exists
    let data_paths = DataPaths::new(&config.data_dir);
    data_paths.ensure_dirs()?;

    let client = Client::try_default().await?;
    let health_router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }));

    let health_server = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await.unwrap();
        axum::serve(listener, health_router).await.unwrap();
    });

    info!("health server listening on :8081");

    // Run reconcilers concurrently
    let channel_config = config.clone();
    let skill_config = config.clone();
    let channel_handle = tokio::spawn(reconcilers::channel::run(client.clone(), channel_config));
    let skill_handle = tokio::spawn(reconcilers::skill::run(client.clone(), skill_config));

    tokio::select! {
        res = channel_handle => { res??; }
        res = skill_handle => { res??; }
        res = health_server => { res?; }
    }

    Ok(())
}
