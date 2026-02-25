use std::sync::Arc;

use anyhow::Result;
use tokio::process::Command;
use tracing::{error, info, warn};

mod config;
mod terminal;

use config::KkConfig;
use kk_core::paths::DataPaths;
use kk_gateway::config::GatewayConfig;
use kk_gateway::launcher::Launcher;
use kk_gateway::launcher::local::LocalLauncher;
use kk_gateway::loops;
use kk_gateway::state::SharedState;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();

    let kk_config = KkConfig::load()?;
    info!(
        data_dir = kk_config.data_dir,
        channels = kk_config.channels.len(),
        "starting kk"
    );

    // 1. Create data directory structure
    let paths = DataPaths::new(&kk_config.data_dir);
    paths.ensure_dirs()?;

    // 2. Build GatewayConfig from kk.yaml values
    let gw_config = build_gateway_config(&kk_config);

    // 3. Create local launcher
    let launcher: Arc<dyn Launcher> = Arc::new(LocalLauncher::new(
        &kk_config.agent_bin,
        &kk_config.data_dir,
    ));

    // 4. Build shared state
    let state = SharedState::new(gw_config, launcher, &paths)?;

    // File-based recovery
    if let Err(e) = state.rebuild_active_jobs_from_files().await {
        warn!(error = %e, "failed to rebuild active jobs from files");
    }

    // 5. Register terminal channel in groups.d
    terminal::register_terminal_group(&paths)?;

    // 6. Spawn connector child processes
    let mut connector_children = Vec::new();
    for channel in &kk_config.channels {
        match spawn_connector(&kk_config, channel, &paths) {
            Ok(child) => {
                info!(channel = channel.name, pid = ?child.id(), "spawned connector");
                connector_children.push((channel.name.clone(), child));
            }
            Err(e) => {
                error!(channel = channel.name, error = %e, "failed to spawn connector");
            }
        }
    }

    // 7. Run gateway loops in-process
    let inbound = tokio::spawn(loops::inbound::run(state.clone()));
    let results = tokio::spawn(loops::results::run(state.clone()));
    let cleanup = tokio::spawn(loops::cleanup::run(state.clone()));
    let state_reload = tokio::spawn(loops::state_reload::run(state.clone()));

    // 8. Spawn connector watchdog (restarts crashed connectors)
    let watchdog = tokio::spawn(watch_connectors(
        connector_children,
        kk_config,
        paths.clone(),
    ));

    // 9. Run terminal connector (foreground — blocks on stdin)
    let terminal = tokio::spawn(terminal::run(paths));

    info!("kk is running — type a message or press Ctrl+C to stop");

    // 10. Wait for shutdown or crash
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl+C, shutting down");
        }
        res = terminal => {
            info!("terminal exited: {:?}", res);
        }
        res = inbound => {
            error!("inbound loop exited: {:?}", res);
        }
        res = results => {
            error!("results loop exited: {:?}", res);
        }
        res = cleanup => {
            error!("cleanup loop exited: {:?}", res);
        }
        res = state_reload => {
            error!("state reload loop exited: {:?}", res);
        }
        res = watchdog => {
            error!("connector watchdog exited: {:?}", res);
        }
    }

    Ok(())
}

fn spawn_connector(
    kk_config: &KkConfig,
    channel: &config::ChannelConfig,
    paths: &DataPaths,
) -> Result<tokio::process::Child> {
    // Ensure outbox/stream dirs exist for this channel
    std::fs::create_dir_all(paths.outbox_dir(&channel.name))?;
    std::fs::create_dir_all(paths.stream_dir(&channel.name))?;

    let mut cmd = Command::new("kk-connector");
    cmd.env("CHANNEL_TYPE", &channel.channel_type)
        .env("CHANNEL_NAME", &channel.name)
        .env("INBOUND_DIR", paths.inbound_dir())
        .env("OUTBOX_DIR", paths.outbox_dir(&channel.name))
        .env("STREAM_DIR", paths.stream_dir(&channel.name))
        .env(
            "GROUPS_D_FILE",
            paths.groups_d_dir().join(format!("{}.json", channel.name)),
        )
        .env("DATA_DIR", &kk_config.data_dir);

    // Pass through channel-specific env vars (tokens, etc.)
    for (key, value) in &channel.env {
        cmd.env(key, value);
    }

    Ok(cmd.spawn()?)
}

/// Watch connector child processes; restart any that crash.
async fn watch_connectors(
    mut children: Vec<(String, tokio::process::Child)>,
    config: KkConfig,
    paths: DataPaths,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        let mut i = 0;
        while i < children.len() {
            let (ref name, ref mut child) = children[i];
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!(
                        channel = name.as_str(),
                        status = %status,
                        "connector exited, restarting"
                    );
                    // Find the channel config and restart
                    if let Some(ch_config) = config.channels.iter().find(|c| c.name == *name) {
                        match spawn_connector(&config, ch_config, &paths) {
                            Ok(new_child) => {
                                info!(channel = name.as_str(), pid = ?new_child.id(), "restarted connector");
                                children[i].1 = new_child;
                            }
                            Err(e) => {
                                error!(channel = name.as_str(), error = %e, "failed to restart connector");
                            }
                        }
                    }
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    warn!(channel = name.as_str(), error = %e, "error checking connector status");
                }
            }
            i += 1;
        }
    }
}

/// Map kk.yaml fields to GatewayConfig.
fn build_gateway_config(kk: &KkConfig) -> GatewayConfig {
    GatewayConfig {
        data_dir: kk.data_dir.clone(),
        namespace: "local".into(),
        image_agent: String::new(),
        api_keys_secret: String::new(),
        job_active_deadline: 0,
        job_ttl_after_finished: 300,
        job_idle_timeout: kk.idle_timeout,
        job_max_turns: kk.max_turns,
        job_cpu_request: String::new(),
        job_cpu_limit: String::new(),
        job_memory_request: String::new(),
        job_memory_limit: String::new(),
        inbound_poll_interval_ms: 2000,
        results_poll_interval_ms: 2000,
        cleanup_interval_ms: 60000,
        stale_message_timeout: 300,
        results_archive_ttl: 86400,
        pvc_claim_name: String::new(),
        state_reload_interval_ms: 30000,
    }
}
