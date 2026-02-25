mod config;
mod platform;

use anyhow::Result;
use axum::{Router, routing::get};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

use kk_core::nq;
use kk_core::types::OutboundMessage;

use crate::config::ConnectorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();

    let config = ConnectorConfig::from_env()?;
    info!(
        channel = config.channel_name,
        channel_type = config.channel_type,
        "starting kk-connector"
    );

    // Ensure queue directories
    std::fs::create_dir_all(&config.inbound_dir)?;
    std::fs::create_dir_all(&config.outbox_dir)?;

    let health_router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }));

    let health_server = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8083").await.unwrap();
        axum::serve(listener, health_router).await.unwrap();
    });

    // TODO: Initialize platform client based on channel_type
    // let platform = platform::create(&config).await?;

    // Outbound loop: poll outbox, send via platform
    let outbox_dir = config.outbox_dir.clone();
    let channel_name = config.channel_name.clone();
    let outbound_handle = tokio::spawn(async move {
        let interval = Duration::from_millis(1000); // Protocol §7: 1s for outbound
        loop {
            if let Err(e) = poll_outbound(&outbox_dir, &channel_name).await {
                error!(error = %e, "outbound poll error");
            }
            sleep(interval).await;
        }
    });

    info!("connector loops started");

    tokio::select! {
        res = outbound_handle => { res?; }
        res = health_server => { res?; }
    }

    Ok(())
}

async fn poll_outbound(outbox_dir: &str, channel_name: &str) -> Result<()> {
    let dir = std::path::Path::new(outbox_dir);
    let files = nq::list_pending(dir)?;

    for file_path in files {
        let raw = match nq::read_message(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "failed to read outbound message");
                continue;
            }
        };

        let msg: OutboundMessage = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                error!(file = %file_path.display(), error = %e, "malformed outbound message, discarding");
                let _ = nq::delete(&file_path);
                continue;
            }
        };

        if msg.channel != channel_name {
            error!(
                expected = channel_name,
                got = msg.channel,
                "outbound channel mismatch, discarding"
            );
            let _ = nq::delete(&file_path);
            continue;
        }

        // TODO: send via platform
        // platform.send(&msg).await?;

        info!(
            group = msg.group,
            text_len = msg.text.len(),
            "sent outbound message"
        );
        let _ = nq::delete(&file_path);
    }

    Ok(())
}
