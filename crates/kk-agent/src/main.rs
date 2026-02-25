use anyhow::Result;
use kk_core::paths::DataPaths;

fn run() -> Result<()> {
    kk_core::logging::init();

    let config = kk_agent::config::AgentConfig::from_env()?;
    let paths = DataPaths::new(&config.data_dir);

    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    std::fs::create_dir_all(&session_dir)?;
    std::fs::create_dir_all(paths.results_dir(&config.session_id))?;

    tracing::info!(
        session_id = %config.session_id,
        group = %config.group,
        "agent starting"
    );

    kk_agent::phases::phase_0_skills(&config, &paths, &session_dir)?;
    kk_agent::phases::phase_1_prompt(&config, &paths, &session_dir)?;
    kk_agent::phases::phase_2_followups(&config, &paths, &session_dir)?;

    // Skip phase 3 if agent was stopped or hit context overflow
    let current_status =
        std::fs::read_to_string(paths.result_status(&config.session_id)).unwrap_or_default();
    let status_str = current_status.trim();
    if status_str != "stopped" && status_str != "overflow" {
        kk_agent::phases::phase_3_done(&paths, &config.session_id)?;
    } else {
        tracing::info!(
            status = status_str,
            "skipping phase 3 (non-done terminal status)"
        );
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("agent error: {e:#}");
        std::process::exit(1);
    }
}
