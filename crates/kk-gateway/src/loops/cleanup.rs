//! Cleanup loop: detects crashed agents, handles stuck results, purges archives.

use std::collections::HashSet;

use anyhow::{Context, Result};
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::nq;

use crate::state::SharedState;

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.cleanup_interval_ms);
    info!(
        interval_ms = state.config.cleanup_interval_ms,
        "cleanup loop started"
    );

    loop {
        if let Err(e) = cleanup_once(&state).await {
            error!(error = %e, "cleanup error");
        }
        sleep(interval).await;
    }
}

pub async fn cleanup_once(state: &SharedState) -> Result<()> {
    // 1. Clean up finished agents (K8s: delete expired Jobs; local: no-op)
    if let Err(e) = state
        .launcher
        .cleanup_finished(state.config.job_ttl_after_finished)
        .await
    {
        warn!(error = %e, "launcher cleanup_finished error");
    }

    // 2. Detect crashed agents (in activeJobs but launcher says gone)
    detect_crashed_jobs(state).await?;

    // 3. Handle orphaned group queue files
    cleanup_orphaned_queues(state)?;

    // 4. Purge archived results older than RESULTS_ARCHIVE_TTL
    purge_old_archives(state)?;

    Ok(())
}

/// Detect agents tracked in activeJobs but no longer running in the launcher.
async fn detect_crashed_jobs(state: &SharedState) -> Result<()> {
    let actual_handles: HashSet<String> = state
        .launcher
        .list_active_handles()
        .await?
        .into_iter()
        .collect();

    let active = state.active_jobs.read().await;
    let stale_keys: Vec<String> = active
        .iter()
        .filter(|(_, aj)| !actual_handles.contains(&aj.job_name))
        .map(|(key, _)| key.clone())
        .collect();
    drop(active);

    if stale_keys.is_empty() {
        return Ok(());
    }

    let mut active = state.active_jobs.write().await;
    for key in &stale_keys {
        if let Some(aj) = active.remove(key) {
            warn!(
                routing_key = key,
                job = aj.job_name,
                session_id = aj.session_id,
                "detected crashed/missing agent, removing from activeJobs"
            );

            // Mark the result as error if status file exists and is still "running"
            let status_path = state.paths.result_status(&aj.session_id);
            if status_path.exists()
                && let Ok(s) = std::fs::read_to_string(&status_path)
                && s.trim() == "running"
            {
                let _ = std::fs::write(&status_path, "error");
            }

            // Clean up stream offset
            state.stream_offsets.write().await.remove(&aj.session_id);
        }
    }

    Ok(())
}

/// Discard orphaned follow-up queue files that are older than stale_message_timeout.
/// These are files left in group queues for Jobs that have already finished.
pub fn cleanup_orphaned_queues(state: &SharedState) -> Result<()> {
    let groups_dir = state.paths.group_queue_dir("");
    if !groups_dir.exists() {
        return Ok(());
    }

    let entries = match std::fs::read_dir(&groups_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        let dir = entry.path();
        let files = match nq::list_pending(&dir) {
            Ok(f) => f,
            Err(_) => continue,
        };

        for file in files {
            match nq::file_age_secs(&file) {
                Ok(age) if age > state.config.stale_message_timeout => {
                    debug!(file = %file.display(), age, "discarding orphaned queue file");
                    let _ = nq::delete(&file);
                }
                _ => {}
            }
        }

        // Also check threads/ subdirectory
        let threads_dir = dir.join("threads");
        if threads_dir.exists()
            && let Ok(thread_entries) = std::fs::read_dir(&threads_dir)
        {
            for te in thread_entries.flatten() {
                if !te.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let thread_dir = te.path();
                if let Ok(files) = nq::list_pending(&thread_dir) {
                    for file in files {
                        if let Ok(age) = nq::file_age_secs(&file)
                            && age > state.config.stale_message_timeout
                        {
                            debug!(file = %file.display(), age, "discarding orphaned thread queue file");
                            let _ = nq::delete(&file);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Purge archived results older than results_archive_ttl.
pub fn purge_old_archives(state: &SharedState) -> Result<()> {
    let done_dir = state.paths.results_done_dir();
    if !done_dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&done_dir)
        .with_context(|| format!("failed to read .done dir: {}", done_dir.display()))?;

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let age = metadata
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if age > state.config.results_archive_ttl {
            let path = entry.path();
            info!(dir = %path.display(), age, "purging old archived result");
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!(dir = %path.display(), error = %e, "failed to purge archived result");
            }
        }
    }

    Ok(())
}
