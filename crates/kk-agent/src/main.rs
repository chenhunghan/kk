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

    kk_agent::phases::phase_0_skills(&paths, &session_dir)?;
    kk_agent::phases::phase_1_prompt(&config, &paths, &session_dir)?;
    kk_agent::phases::phase_2_followups(&config, &paths, &session_dir)?;
    kk_agent::phases::phase_3_done(&paths, &config.session_id)?;

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("agent error: {e:#}");
        std::process::exit(1);
    }
}
