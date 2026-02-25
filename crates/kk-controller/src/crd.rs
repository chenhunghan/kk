use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Channel CRD — declares a messaging platform connection.
/// The Controller creates a Connector Deployment for each Channel.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[kube(
    group = "kk.io",
    version = "v1alpha1",
    kind = "Channel",
    plural = "channels",
    status = "ChannelStatus",
    namespaced
)]
pub struct ChannelSpec {
    /// Platform type: telegram, whatsapp, slack, discord, signal
    #[serde(rename = "type")]
    pub channel_type: String,

    /// Name of the K8s Secret containing platform credentials
    #[serde(rename = "secretRef")]
    pub secret_ref: String,

    /// Optional platform-specific configuration
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, JsonSchema)]
pub struct ChannelStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "connectorPod")]
    pub connector_pod: String,
}

/// Skill CRD — declares a skill to fetch from a git repo.
/// The Controller clones the repo, validates SKILL.md, and copies to PVC.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[kube(
    group = "kk.io",
    version = "v1alpha1",
    kind = "Skill",
    plural = "skills",
    shortname = "sk",
    status = "SkillStatus",
    namespaced
)]
pub struct SkillSpec {
    /// GitHub shorthand path: owner/repo/path/to/skill
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, JsonSchema)]
pub struct SkillStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "skillName")]
    pub skill_name: String,
    #[serde(default, rename = "skillDescription")]
    pub skill_description: String,
    #[serde(default, rename = "lastFetched")]
    pub last_fetched: String,
}
