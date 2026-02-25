//! Cleanup loop: deletes finished K8s Jobs, handles stuck results, purges archives.

use std::collections::HashSet;

use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::Job;
use kube::Api;
use kube::api::{DeleteParams, ListParams};
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
    // 1. Clean up completed/failed K8s Jobs past TTL
    cleanup_finished_jobs(state).await?;

    // 2. Detect crashed Jobs (in activeJobs but K8s Job gone)
    detect_crashed_jobs(state).await?;

    // 3. Handle orphaned group queue files
    cleanup_orphaned_queues(state)?;

    // 4. Purge archived results older than RESULTS_ARCHIVE_TTL
    purge_old_archives(state)?;

    Ok(())
}

/// Delete completed/failed K8s Jobs whose TTL has expired.
/// K8s ttlSecondsAfterFinished should handle most of this, but we
/// clean up any that slip through.
async fn cleanup_finished_jobs(state: &SharedState) -> Result<()> {
    let jobs_api: Api<Job> = Api::namespaced(state.client.clone(), &state.config.namespace);
    let lp = ListParams::default().labels("app=kk-agent");
    let job_list = jobs_api.list(&lp).await?;

    for job in job_list.items {
        let name = job.metadata.name.clone().unwrap_or_default();
        let status = match &job.status {
            Some(s) => s,
            None => continue,
        };

        let is_finished = status.succeeded.unwrap_or(0) > 0 || status.failed.unwrap_or(0) > 0;
        if !is_finished {
            continue;
        }

        // Check if completion time is past TTL
        let completion_time = status
            .completion_time
            .as_ref()
            .map(|t| t.0.timestamp() as u64)
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if now.saturating_sub(completion_time) > state.config.job_ttl_after_finished {
            info!(job = name, "deleting expired Job");
            if let Err(e) = jobs_api.delete(&name, &DeleteParams::default()).await {
                warn!(job = name, error = %e, "failed to delete expired Job");
            }
        }
    }

    Ok(())
}

/// Detect Jobs tracked in activeJobs but no longer existing in K8s.
/// This handles cases where a Job was deleted externally or crashed.
async fn detect_crashed_jobs(state: &SharedState) -> Result<()> {
    let jobs_api: Api<Job> = Api::namespaced(state.client.clone(), &state.config.namespace);
    let lp = ListParams::default().labels("app=kk-agent");
    let job_list = jobs_api.list(&lp).await?;

    // Build set of actual running Job names
    let actual_jobs: HashSet<String> = job_list
        .items
        .iter()
        .filter_map(|j| j.metadata.name.clone())
        .collect();

    let active = state.active_jobs.read().await;
    let stale_keys: Vec<String> = active
        .iter()
        .filter(|(_, aj)| !actual_jobs.contains(&aj.job_name))
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
                "detected crashed/missing Job, removing from activeJobs"
            );

            // Mark the result as error if status file exists and is still "running"
            let status_path = state.paths.result_status(&aj.session_id);
            if status_path.exists()
                && let Ok(s) = std::fs::read_to_string(&status_path)
                && s.trim() == "running"
            {
                let _ = std::fs::write(&status_path, "error");
            }
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
