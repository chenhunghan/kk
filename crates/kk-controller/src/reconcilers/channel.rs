use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvFromSource, EnvVar, PodSpec, PodTemplateSpec, Secret, SecretEnvSource, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kk_core::paths::DataPaths;
use kube::Client;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use tracing::{error, info, warn};

use crate::config::ControllerConfig;
use crate::crd::Channel;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("missing metadata: {0}")]
    MissingMeta(&'static str),
}

pub struct Ctx {
    pub client: Client,
    pub config: ControllerConfig,
}

/// Convert a camelCase or kebab-case string to UPPER_SNAKE_CASE.
pub fn to_upper_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        if ch == '-' || ch == '.' {
            result.push('_');
        } else {
            result.push(ch.to_ascii_uppercase());
        }
    }
    result
}

/// Build the desired Connector Deployment for a Channel CR.
/// Pure function — no I/O, fully unit-testable.
pub fn build_connector_deployment(channel: &Channel, config: &ControllerConfig) -> Deployment {
    let ch_name = channel.metadata.name.as_deref().unwrap_or("unknown");
    let ns = channel
        .metadata
        .namespace
        .as_deref()
        .unwrap_or(&config.namespace);
    let deploy_name = format!("connector-{ch_name}");

    let labels: BTreeMap<String, String> = BTreeMap::from([
        ("app".into(), "kk-connector".into()),
        ("channel".into(), ch_name.into()),
        ("channel-type".into(), channel.spec.channel_type.clone()),
    ]);

    let owner_ref = OwnerReference {
        api_version: "kk.io/v1alpha1".into(),
        kind: "Channel".into(),
        name: ch_name.into(),
        uid: channel.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    };

    let data_paths = DataPaths::new(&config.data_dir);
    let mut env_vars = vec![
        EnvVar {
            name: "CHANNEL_TYPE".into(),
            value: Some(channel.spec.channel_type.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "CHANNEL_NAME".into(),
            value: Some(ch_name.into()),
            ..Default::default()
        },
        EnvVar {
            name: "INBOUND_DIR".into(),
            value: Some(data_paths.inbound_dir().to_string_lossy().into_owned()),
            ..Default::default()
        },
        EnvVar {
            name: "OUTBOX_DIR".into(),
            value: Some(
                data_paths
                    .outbox_dir(ch_name)
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        },
        EnvVar {
            name: "GROUPS_D_FILE".into(),
            value: Some(
                data_paths
                    .groups_d_dir()
                    .join(format!("{ch_name}.json"))
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        },
    ];

    // Merge channel.spec.config keys as CONFIG_UPPER_SNAKE env vars
    if let Some(serde_json::Value::Object(map)) = &channel.spec.config {
        for (key, val) in map {
            let env_name = format!("CONFIG_{}", to_upper_snake(key));
            let env_value = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            env_vars.push(EnvVar {
                name: env_name,
                value: Some(env_value),
                ..Default::default()
            });
        }
    }

    let env_from = vec![EnvFromSource {
        secret_ref: Some(SecretEnvSource {
            name: channel.spec.secret_ref.clone(),
            optional: Some(false),
        }),
        ..Default::default()
    }];

    let container = Container {
        name: "connector".into(),
        image: Some(config.image_connector.clone()),
        env: Some(env_vars),
        env_from: Some(env_from),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".into(),
            mount_path: config.data_dir.clone(),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let volume = Volume {
        name: "data".into(),
        persistent_volume_claim: Some(
            k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                claim_name: config.pvc_claim_name.clone(),
                read_only: Some(false),
            },
        ),
        ..Default::default()
    };

    Deployment {
        metadata: ObjectMeta {
            name: Some(deploy_name),
            namespace: Some(ns.into()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([
                    ("app".into(), "kk-connector".into()),
                    ("channel".into(), ch_name.into()),
                ])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    volumes: Some(vec![volume]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn update_channel_status(
    client: &Client,
    channel: &Channel,
    phase: &str,
    message: &str,
    connector_pod: &str,
) -> Result<(), Error> {
    let ns = channel.metadata.namespace.as_deref().unwrap_or("default");
    let name = channel
        .metadata
        .name
        .as_deref()
        .ok_or(Error::MissingMeta("name"))?;
    let channels: Api<Channel> = Api::namespaced(client.clone(), ns);

    let status = serde_json::json!({
        "status": {
            "phase": phase,
            "message": message,
            "connectorPod": connector_pod,
        }
    });

    channels
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&status))
        .await?;

    Ok(())
}

/// Determine channel phase from a Deployment's status.
fn determine_deployment_phase(deploy: &Deployment) -> (String, String, String) {
    let status = match &deploy.status {
        Some(s) => s,
        None => {
            return (
                "Pending".into(),
                "Waiting for deployment".into(),
                String::new(),
            );
        }
    };

    let ready = status.ready_replicas.unwrap_or(0);
    if ready >= 1 {
        return (
            "Connected".into(),
            "Connector running".into(),
            String::new(),
        );
    }

    if let Some(conditions) = &status.conditions {
        for cond in conditions {
            if cond.type_ == "Available" && cond.status == "False" {
                let msg = cond.message.clone().unwrap_or_default();
                if msg.contains("CrashLoopBackOff") || msg.contains("Error") {
                    return ("Error".into(), msg, String::new());
                }
            }
        }
    }

    (
        "Pending".into(),
        "Waiting for connector to become ready".into(),
        String::new(),
    )
}

pub async fn reconcile(channel: Arc<Channel>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let name = channel.metadata.name.as_deref().unwrap_or("unknown");
    let ns = channel.metadata.namespace.as_deref().unwrap_or("default");
    info!(channel = name, namespace = ns, "reconciling channel");

    // 1. Validate secret exists
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    match secrets.get(&channel.spec.secret_ref).await {
        Ok(_) => {}
        Err(kube::Error::Api(resp)) if resp.code == 404 => {
            warn!(
                channel = name,
                secret = channel.spec.secret_ref.as_str(),
                "secret not found"
            );
            update_channel_status(
                &ctx.client,
                &channel,
                "Error",
                &format!("Secret '{}' not found", channel.spec.secret_ref),
                "",
            )
            .await?;
            return Ok(Action::requeue(std::time::Duration::from_secs(30)));
        }
        Err(e) => return Err(e.into()),
    }

    // 2. Build desired Deployment
    let desired = build_connector_deployment(&channel, &ctx.config);
    let deploy_name = format!("connector-{name}");

    // 3. Server-side apply
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    let params = PatchParams::apply("kk-controller").force();
    let applied = deployments
        .patch(&deploy_name, &params, &Patch::Apply(&desired))
        .await?;

    // 4. Ensure outbox directory exists
    let data_paths = DataPaths::new(&ctx.config.data_dir);
    std::fs::create_dir_all(data_paths.outbox_dir(name))?;

    // 5. Check deployment readiness → update status
    let (phase, message, pod_name) = determine_deployment_phase(&applied);
    update_channel_status(&ctx.client, &channel, &phase, &message, &pod_name).await?;

    info!(channel = name, phase = phase.as_str(), "channel reconciled");
    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

fn error_policy(channel: Arc<Channel>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    let name = channel.metadata.name.as_deref().unwrap_or("unknown");
    error!(channel = name, error = %err, "channel reconciliation error");
    Action::requeue(std::time::Duration::from_secs(30))
}

pub async fn run(client: Client, config: ControllerConfig) -> anyhow::Result<()> {
    let ns = config.namespace.clone();
    let channels: Api<Channel> = Api::namespaced(client.clone(), &ns);
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        config,
    });

    info!("starting channel controller");

    Controller::new(channels, watcher::Config::default())
        .owns(deployments, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => error!(error = %e, "channel controller error"),
            }
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::ChannelSpec;

    fn test_config() -> ControllerConfig {
        ControllerConfig {
            image_connector: "kk-connector:test".into(),
            data_dir: "/data".into(),
            namespace: "test-ns".into(),
            skill_clone_timeout: 60,
            pvc_claim_name: "kk-data".into(),
        }
    }

    fn test_channel(name: &str, channel_type: &str, secret_ref: &str) -> Channel {
        Channel {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("test-ns".into()),
                uid: Some("test-uid-1234".into()),
                ..Default::default()
            },
            spec: ChannelSpec {
                channel_type: channel_type.into(),
                secret_ref: secret_ref.into(),
                config: None,
            },
            status: None,
        }
    }

    #[test]
    fn test_to_upper_snake() {
        assert_eq!(to_upper_snake("botToken"), "BOT_TOKEN");
        assert_eq!(to_upper_snake("chatId"), "CHAT_ID");
        assert_eq!(to_upper_snake("simple"), "SIMPLE");
        assert_eq!(to_upper_snake("allowedChatIds"), "ALLOWED_CHAT_IDS");
        assert_eq!(to_upper_snake("some-key"), "SOME_KEY");
        assert_eq!(to_upper_snake("some.key"), "SOME_KEY");
    }

    #[test]
    fn test_build_connector_deployment_basic() {
        let config = test_config();
        let channel = test_channel("telegram-bot-1", "telegram", "tg-secret");
        let deploy = build_connector_deployment(&channel, &config);

        assert_eq!(
            deploy.metadata.name.as_deref(),
            Some("connector-telegram-bot-1")
        );
        assert_eq!(deploy.metadata.namespace.as_deref(), Some("test-ns"));

        let owner_refs = deploy.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owner_refs.len(), 1);
        assert_eq!(owner_refs[0].kind, "Channel");
        assert_eq!(owner_refs[0].name, "telegram-bot-1");
        assert!(owner_refs[0].controller.unwrap());
        assert!(owner_refs[0].block_owner_deletion.unwrap());

        let labels = deploy.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["app"], "kk-connector");
        assert_eq!(labels["channel"], "telegram-bot-1");
        assert_eq!(labels["channel-type"], "telegram");

        let spec = deploy.spec.as_ref().unwrap();
        let pod_spec = spec.template.spec.as_ref().unwrap();
        let container = &pod_spec.containers[0];
        assert_eq!(container.image.as_deref(), Some("kk-connector:test"));

        let env = container.env.as_ref().unwrap();
        let env_map: BTreeMap<&str, &str> = env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(env_map["CHANNEL_TYPE"], "telegram");
        assert_eq!(env_map["CHANNEL_NAME"], "telegram-bot-1");
        assert_eq!(env_map["INBOUND_DIR"], "/data/inbound");
        assert_eq!(env_map["OUTBOX_DIR"], "/data/outbox/telegram-bot-1");

        let env_from = container.env_from.as_ref().unwrap();
        assert_eq!(env_from[0].secret_ref.as_ref().unwrap().name, "tg-secret");

        let mounts = container.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts[0].mount_path, "/data");

        let volumes = pod_spec.volumes.as_ref().unwrap();
        assert_eq!(
            volumes[0]
                .persistent_volume_claim
                .as_ref()
                .unwrap()
                .claim_name,
            "kk-data"
        );
    }

    #[test]
    fn test_build_connector_deployment_with_config() {
        let config = test_config();
        let mut channel = test_channel("slack-bot", "slack", "slack-secret");
        channel.spec.config = Some(serde_json::json!({
            "botToken": "xoxb-test",
            "allowedChannels": "general,random"
        }));

        let deploy = build_connector_deployment(&channel, &config);
        let container = &deploy
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];

        let env = container.env.as_ref().unwrap();
        let env_map: BTreeMap<&str, &str> = env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or("")))
            .collect();

        assert_eq!(env_map["CONFIG_BOT_TOKEN"], "xoxb-test");
        assert_eq!(env_map["CONFIG_ALLOWED_CHANNELS"], "general,random");
    }
}
