mod config;
mod loops;
mod state;

use anyhow::Result;
use axum::{Router, routing::get};
use kube::Client;
use tracing::info;

use crate::config::GatewayConfig;
use crate::state::SharedState;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();
    info!("starting kk-gateway");

    let config = GatewayConfig::from_env();
    let client = Client::try_default().await?;
    let paths = kk_core::paths::DataPaths::new(&config.data_dir);
    paths.ensure_dirs()?;

    let state = SharedState::new(config.clone(), client.clone(), &paths)?;

    let shared = state.clone();
    let health_router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route(
            "/status",
            get(move || {
                let s = shared.clone();
                async move { s.status_json().await }
            }),
        );

    let health_server = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8082").await.unwrap();
        axum::serve(listener, health_router).await.unwrap();
    });

    info!("health server listening on :8082");

    let inbound = tokio::spawn(loops::inbound::run(state.clone()));
    let results = tokio::spawn(loops::results::run(state.clone()));
    let cleanup = tokio::spawn(loops::cleanup::run(state.clone()));

    tokio::select! {
        res = inbound => { res??; }
        res = results => { res??; }
        res = cleanup => { res??; }
        res = health_server => { res?; }
    }

    Ok(())
}
