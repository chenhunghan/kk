//! K8s Job launcher: creates/manages agent Jobs via the Kubernetes API.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvFromSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
    ResourceRequirements, SecretEnvSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use tracing::{info, warn};

use crate::config::GatewayConfig;
use kk_core::types::InboundMessage;

use super::{LaunchedAgent, Launcher, now_secs};

pub struct K8sLauncher {
    client: Client,
    namespace: String,
}

impl K8sLauncher {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
        }
    }
}

#[async_trait]
impl Launcher for K8sLauncher {
    async fn launch(
        &self,
        job_name: &str,
        session_id: &str,
        msg: &InboundMessage,
        config: &GatewayConfig,
    ) -> Result<LaunchedAgent> {
        let job = build_agent_job(job_name, session_id, msg, config);
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        jobs_api
            .create(&PostParams::default(), &job)
            .await
            .with_context(|| format!("failed to create Job {job_name}"))?;

        Ok(LaunchedAgent {
            handle: job_name.to_string(),
            session_id: session_id.to_string(),
            group: msg.group.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: now_secs(),
        })
    }

    async fn list_active_handles(&self) -> Result<Vec<String>> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels("app=kk-agent");
        let job_list = jobs_api.list(&lp).await?;

        Ok(job_list
            .items
            .iter()
            .filter(|j| {
                // Only include non-completed Jobs
                j.status
                    .as_ref()
                    .map(|s| s.succeeded.unwrap_or(0) == 0 && s.failed.unwrap_or(0) == 0)
                    .unwrap_or(true)
            })
            .filter_map(|j| j.metadata.name.clone())
            .collect())
    }

    async fn kill(&self, handle: &str) -> Result<()> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = jobs_api.delete(handle, &DeleteParams::default()).await;
        Ok(())
    }

    async fn cleanup_finished(&self, ttl: u64) -> Result<()> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
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

            let completion_time = status
                .completion_time
                .as_ref()
                .map(|t| t.0.timestamp() as u64)
                .unwrap_or(0);

            if now_secs().saturating_sub(completion_time) > ttl {
                info!(job = name, "deleting expired Job");
                if let Err(e) = jobs_api.delete(&name, &DeleteParams::default()).await {
                    warn!(job = name, error = %e, "failed to delete expired Job");
                }
            }
        }

        Ok(())
    }

    async fn recover(&self) -> Result<Vec<LaunchedAgent>> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels("app=kk-agent");
        let job_list = jobs_api
            .list(&lp)
            .await
            .context("failed to list agent Jobs")?;

        let mut agents = Vec::new();

        for job in job_list.items {
            let labels = job.metadata.labels.as_ref();
            let name = job.metadata.name.clone().unwrap_or_default();

            // Skip completed/failed Jobs
            if let Some(status) = &job.status
                && (status.succeeded.unwrap_or(0) > 0 || status.failed.unwrap_or(0) > 0)
            {
                continue;
            }

            let group = labels
                .and_then(|l| l.get("kk.io/group"))
                .cloned()
                .unwrap_or_default();
            let session_id = labels
                .and_then(|l| l.get("kk.io/session-id"))
                .cloned()
                .unwrap_or_default();
            let thread_id = labels.and_then(|l| l.get("kk.io/thread-id")).cloned();

            if group.is_empty() || session_id.is_empty() {
                warn!(job = name, "agent Job missing required labels, skipping");
                continue;
            }

            let created_at = job
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.timestamp() as u64)
                .unwrap_or(0);

            agents.push(LaunchedAgent {
                handle: name,
                session_id,
                group,
                thread_id,
                created_at,
            });
        }

        Ok(agents)
    }
}

/// Build a K8s Job spec for the agent.
fn build_agent_job(
    job_name: &str,
    session_id: &str,
    msg: &InboundMessage,
    config: &GatewayConfig,
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
        EnvVar {
            name: "AGENT_TYPE".to_string(),
            value: Some(config.agent_type.clone()),
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
