//! Inbound loop: polls /data/inbound/ and routes messages.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvFromSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
    ResourceRequirements, SecretEnvSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Api;
use kube::api::PostParams;
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

use kk_core::nq;
use kk_core::types::{
    FollowUpMessage, InboundMessage, RequestManifest, RequestMessage, TriggerMode,
};

use crate::state::{self, ActiveJob, SharedState};

pub async fn run(state: SharedState) -> Result<()> {
    let interval = Duration::from_millis(state.config.inbound_poll_interval_ms);
    info!(
        interval_ms = state.config.inbound_poll_interval_ms,
        "inbound loop started"
    );

    loop {
        if let Err(e) = poll_once(&state).await {
            error!(error = %e, "inbound poll error");
        }
        sleep(interval).await;
    }
}

pub async fn poll_once(state: &SharedState) -> Result<()> {
    let inbound_dir = state.paths.inbound_dir();
    let files = nq::list_pending(&inbound_dir)?;

    if files.is_empty() {
        return Ok(());
    }

    for file_path in files {
        let raw = match nq::read_message(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "failed to read inbound message");
                continue;
            }
        };

        let msg: InboundMessage = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                warn!(file = %file_path.display(), error = %e, "invalid inbound JSON, discarding");
                let _ = nq::delete(&file_path);
                continue;
            }
        };

        // Check staleness
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now.saturating_sub(msg.timestamp) > state.config.stale_message_timeout {
            warn!(file = %file_path.display(), age = now - msg.timestamp, "stale inbound message, discarding");
            let _ = nq::delete(&file_path);
            continue;
        }

        // Route the message
        if let Err(e) = route_message(state, &msg).await {
            error!(group = msg.group, error = %e, "failed to route message");
        }

        // Delete processed file
        let _ = nq::delete(&file_path);
    }

    Ok(())
}

async fn route_message(state: &SharedState, msg: &InboundMessage) -> Result<()> {
    let groups = state.groups_config.read().await;
    let group_config = match groups.groups.get(&msg.group) {
        Some(config) => config,
        None => {
            debug!(
                group = msg.group,
                "message from unregistered group, skipping"
            );
            return Ok(());
        }
    };

    // Check trigger
    let should_trigger = match group_config.trigger_mode {
        TriggerMode::Always => true,
        TriggerMode::Mention => group_config
            .trigger_pattern
            .as_ref()
            .map(|p| msg.text.contains(p.as_str()))
            .unwrap_or(false),
        TriggerMode::Prefix => group_config
            .trigger_pattern
            .as_ref()
            .map(|p| msg.text.starts_with(p.as_str()))
            .unwrap_or(false),
    };

    if !should_trigger {
        debug!(
            group = msg.group,
            trigger_mode = ?group_config.trigger_mode,
            "message did not match trigger, skipping"
        );
        return Ok(());
    }

    // Build thread-aware routing key
    let key = state::routing_key(&msg.group, msg.thread_id.as_deref());

    let active_jobs = state.active_jobs.read().await;
    if active_jobs.contains_key(&key) {
        drop(active_jobs);
        hot_path(state, msg, &key).await
    } else {
        drop(active_jobs);
        // Strip trigger pattern from prompt text
        let prompt = strip_trigger(
            &msg.text,
            &group_config.trigger_mode,
            group_config.trigger_pattern.as_deref(),
        );
        cold_path(state, msg, &key, &prompt).await
    }
}

/// Strip trigger pattern from message text.
fn strip_trigger(text: &str, trigger_mode: &TriggerMode, trigger_pattern: Option<&str>) -> String {
    match (trigger_mode, trigger_pattern) {
        (TriggerMode::Prefix, Some(pattern)) => text
            .strip_prefix(pattern)
            .unwrap_or(text)
            .trim()
            .to_string(),
        (TriggerMode::Mention, Some(pattern)) => text.replace(pattern, "").trim().to_string(),
        _ => text.to_string(),
    }
}

/// Cold path: no active Job for this routing key — create a new Agent Job.
async fn cold_path(
    state: &SharedState,
    msg: &InboundMessage,
    routing_key: &str,
    prompt: &str,
) -> Result<()> {
    let session_id =
        kk_core::paths::session_id_threaded(&msg.group, msg.thread_id.as_deref(), msg.timestamp);
    let job_name = kk_core::paths::job_name(&session_id);

    info!(
        group = msg.group,
        session_id, job_name, "COLD PATH: creating Agent Job"
    );

    // 1. Create results directory + request.json
    let results_dir = state.paths.results_dir(&session_id);
    std::fs::create_dir_all(&results_dir)
        .with_context(|| format!("failed to create results dir: {}", results_dir.display()))?;

    let manifest = RequestManifest {
        channel: msg.channel.clone(),
        group: msg.group.clone(),
        thread_id: msg.thread_id.clone(),
        sender: msg.sender.clone(),
        meta: msg.meta.clone(),
        messages: vec![RequestMessage {
            sender: msg.sender.clone(),
            text: prompt.to_string(),
            ts: msg.timestamp,
        }],
    };
    let manifest_path = state.paths.request_manifest(&session_id);
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("failed to write request.json: {}", manifest_path.display()))?;

    // Write initial status
    let status_path = state.paths.result_status(&session_id);
    std::fs::write(&status_path, "running")?;

    // 2. Ensure per-group queue directory exists (thread-aware)
    let queue_dir = state
        .paths
        .group_queue_dir_threaded(&msg.group, msg.thread_id.as_deref());
    std::fs::create_dir_all(&queue_dir)?;

    // 3. Create K8s Job
    let job = build_agent_job(&job_name, &session_id, msg, &state.config);
    let jobs_api: Api<Job> = Api::namespaced(state.client.clone(), &state.config.namespace);
    jobs_api
        .create(&PostParams::default(), &job)
        .await
        .with_context(|| format!("failed to create Job {job_name}"))?;

    // 4. Update activeJobs
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut active = state.active_jobs.write().await;
    active.insert(
        routing_key.to_string(),
        ActiveJob {
            job_name: job_name.clone(),
            session_id,
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: now,
        },
    );

    info!(job = job_name, routing_key, "Agent Job created");
    Ok(())
}

/// Hot path: an Agent Job is already running — forward as follow-up.
async fn hot_path(state: &SharedState, msg: &InboundMessage, routing_key: &str) -> Result<()> {
    let active_jobs = state.active_jobs.read().await;
    let job_info = active_jobs.get(routing_key).cloned();
    drop(active_jobs);

    let job_info = match job_info {
        Some(j) => j,
        None => return Ok(()),
    };

    info!(
        group = msg.group,
        job_name = job_info.job_name,
        "HOT PATH: forwarding to running Job"
    );

    // 1. Build follow-up message
    let follow_up = FollowUpMessage {
        sender: msg.sender.clone(),
        text: msg.text.clone(),
        timestamp: msg.timestamp,
        channel: msg.channel.clone(),
        thread_id: msg.thread_id.clone(),
        meta: msg.meta.clone(),
    };

    // 2. Atomic write to per-group queue
    let queue_dir = state
        .paths
        .group_queue_dir_threaded(&msg.group, msg.thread_id.as_deref());
    let payload = serde_json::to_vec(&follow_up)?;
    nq::enqueue(&queue_dir, msg.timestamp, &payload)?;

    // 3. Append to request.json messages array (bookkeeping)
    let manifest_path = state.paths.request_manifest(&job_info.session_id);
    if manifest_path.exists()
        && let Ok(content) = std::fs::read_to_string(&manifest_path)
        && let Ok(mut manifest) = serde_json::from_str::<RequestManifest>(&content)
    {
        manifest.messages.push(RequestMessage {
            sender: msg.sender.clone(),
            text: msg.text.clone(),
            ts: msg.timestamp,
        });
        let _ = std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?);
    }

    Ok(())
}

/// Build a K8s Job spec for the agent.
fn build_agent_job(
    job_name: &str,
    session_id: &str,
    msg: &InboundMessage,
    config: &crate::config::GatewayConfig,
) -> Job {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "kk-agent".to_string());
    labels.insert("kk.io/group".to_string(), msg.group.clone());
    labels.insert("kk.io/session-id".to_string(), session_id.to_string());
    if let Some(ref tid) = msg.thread_id {
        labels.insert("kk.io/thread-id".to_string(), tid.clone());
    }

    let mut env = vec![
        EnvVar {
            name: "SESSION_ID".to_string(),
            value: Some(session_id.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "GROUP".to_string(),
            value: Some(msg.group.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "DATA_DIR".to_string(),
            value: Some(config.data_dir.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "IDLE_TIMEOUT".to_string(),
            value: Some(config.job_idle_timeout.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "MAX_TURNS".to_string(),
            value: Some(config.job_max_turns.to_string()),
            ..Default::default()
        },
    ];

    if let Some(ref tid) = msg.thread_id {
        env.push(EnvVar {
            name: "THREAD_ID".to_string(),
            value: Some(tid.clone()),
            ..Default::default()
        });
    }

    let container = Container {
        name: "agent".to_string(),
        image: Some(config.image_agent.clone()),
        env: Some(env),
        env_from: Some(vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource {
                name: config.api_keys_secret.clone(),
                optional: Some(true),
            }),
            ..Default::default()
        }]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.job_cpu_request.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.job_memory_request.clone()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.job_cpu_limit.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.job_memory_limit.clone()),
                ),
            ])),
            ..Default::default()
        }),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".to_string(),
            mount_path: "/data".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    };

    Job {
        metadata: kube::core::ObjectMeta {
            name: Some(job_name.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(JobSpec {
            active_deadline_seconds: Some(config.job_active_deadline as i64),
            ttl_seconds_after_finished: Some(config.job_ttl_after_finished as i32),
            backoff_limit: Some(0),
            template: PodTemplateSpec {
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![container],
                    volumes: Some(vec![Volume {
                        name: "data".to_string(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: config.pvc_claim_name.clone(),
                            read_only: Some(false),
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_trigger_prefix() {
        let result = strip_trigger("/ask what is rust?", &TriggerMode::Prefix, Some("/ask"));
        assert_eq!(result, "what is rust?");
    }

    #[test]
    fn test_strip_trigger_mention() {
        let result = strip_trigger(
            "hey @bot what is rust?",
            &TriggerMode::Mention,
            Some("@bot"),
        );
        assert_eq!(result, "hey  what is rust?");
    }

    #[test]
    fn test_strip_trigger_always() {
        let result = strip_trigger("what is rust?", &TriggerMode::Always, None);
        assert_eq!(result, "what is rust?");
    }

    #[test]
    fn test_build_agent_job() {
        let msg = InboundMessage {
            channel: "telegram-bot-1".to_string(),
            channel_type: kk_core::types::ChannelType::Telegram,
            group: "family-chat".to_string(),
            thread_id: Some("42".to_string()),
            sender: "alice".to_string(),
            text: "hello".to_string(),
            timestamp: 1708801290,
            meta: serde_json::json!({}),
        };

        let config = crate::config::GatewayConfig::from_env();
        let job = build_agent_job(
            "agent-family-chat-t42-1708801290",
            "family-chat-t42-1708801290",
            &msg,
            &config,
        );

        let meta = &job.metadata;
        assert_eq!(
            meta.name.as_deref(),
            Some("agent-family-chat-t42-1708801290")
        );
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels["app"], "kk-agent");
        assert_eq!(labels["kk.io/group"], "family-chat");
        assert_eq!(labels["kk.io/thread-id"], "42");

        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(0));

        let pod_spec = spec.template.spec.as_ref().unwrap();
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(pod_spec.containers.len(), 1);

        let container = &pod_spec.containers[0];
        let env = container.env.as_ref().unwrap();
        let session_env = env.iter().find(|e| e.name == "SESSION_ID").unwrap();
        assert_eq!(
            session_env.value.as_deref(),
            Some("family-chat-t42-1708801290")
        );
        let thread_env = env.iter().find(|e| e.name == "THREAD_ID").unwrap();
        assert_eq!(thread_env.value.as_deref(), Some("42"));
    }

    #[test]
    fn test_build_agent_job_no_thread() {
        let msg = InboundMessage {
            channel: "slack-1".to_string(),
            channel_type: kk_core::types::ChannelType::Slack,
            group: "eng".to_string(),
            thread_id: None,
            sender: "bob".to_string(),
            text: "deploy".to_string(),
            timestamp: 1000,
            meta: serde_json::json!({}),
        };

        let config = crate::config::GatewayConfig::from_env();
        let job = build_agent_job("agent-eng-1000", "eng-1000", &msg, &config);

        let labels = job.metadata.labels.as_ref().unwrap();
        assert!(!labels.contains_key("kk.io/thread-id"));

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(env.iter().all(|e| e.name != "THREAD_ID"));
    }
}
