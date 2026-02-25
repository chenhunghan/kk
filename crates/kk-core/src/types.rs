use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Inbound message written by Connector to /data/inbound/ (Protocol §4.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub channel_type: ChannelType,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub sender: String,
    pub text: String,
    pub timestamp: u64,
    pub meta: serde_json::Value,
}

/// Outbound message written by Gateway to /data/outbox/{channel}/ (Protocol §4.2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub text: String,
    pub meta: serde_json::Value,
}

/// Follow-up message written by Gateway to /data/groups/{group}/ (Protocol §4.3)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowUpMessage {
    pub sender: String,
    pub text: String,
    pub timestamp: u64,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub meta: serde_json::Value,
}

/// Request manifest written by Gateway to /data/results/{session-id}/request.json (Protocol §4.4)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestManifest {
    pub channel: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub sender: String,
    pub meta: serde_json::Value,
    pub messages: Vec<RequestMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMessage {
    pub sender: String,
    pub text: String,
    pub ts: u64,
}

/// Result status (Protocol §4.5)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Running,
    Done,
    Error,
    Stopped,
    Overflow,
}

impl ResultStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Stopped => "stopped",
            Self::Overflow => "overflow",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "error" => Some(Self::Error),
            "stopped" => Some(Self::Stopped),
            "overflow" => Some(Self::Overflow),
            _ => None,
        }
    }
}

/// Supported channel types
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelType {
    Telegram,
    Whatsapp,
    Slack,
    Discord,
    Github,
    Signal,
}

/// Group configuration from /data/state/groups.json (Protocol §6).
/// Keys are group slugs (`[a-z0-9-]+`, e.g. `family-chat`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupsConfig {
    pub groups: HashMap<String, GroupEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupEntry {
    pub trigger_pattern: Option<String>,
    pub trigger_mode: TriggerMode,
    pub channels: HashMap<String, ChannelMapping>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerMode {
    Always,
    Mention,
    Prefix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMapping {
    pub chat_id: String,
}

/// JSONL result line from Agent Job (Protocol §4.6)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultLine {
    #[serde(rename = "type")]
    pub line_type: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub message: Option<AssistantMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub role: String,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
