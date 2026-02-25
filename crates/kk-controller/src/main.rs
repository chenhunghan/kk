mod crd;
mod reconcilers;

use anyhow::Result;
use axum::{Router, routing::get};
use kube::Client;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();
    info!("starting kk-controller");

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
    let channel_handle = tokio::spawn(reconcilers::channel::run(client.clone()));
    let skill_handle = tokio::spawn(reconcilers::skill::run(client.clone()));

    tokio::select! {
        res = channel_handle => { res??; }
        res = skill_handle => { res??; }
        res = health_server => { res?; }
    }

    Ok(())
}
