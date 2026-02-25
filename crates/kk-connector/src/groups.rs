use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use kk_core::types::{ChannelMapping, GroupEntry, GroupsConfig, TriggerMode};

/// Reverse map from provider chat_id → group slug for a specific channel.
#[derive(Debug, Clone)]
pub struct GroupMap {
    map: HashMap<String, String>,
}

/// Generate an auto-registration slug from a chat_id and channel type prefix.
/// Strips the leading `-` (Telegram supergroup IDs are negative) and lowercases.
/// E.g. `tg`, `-1001234567890` → `tg-1001234567890`.
/// E.g. `slack`, `C0123456789` → `slack-c0123456789`.
fn auto_group_slug(chat_id: &str, prefix: &str) -> String {
    let stripped = chat_id.trim_start_matches('-');
    format!("{prefix}-{}", stripped.to_lowercase())
}

/// Load a GroupsConfig from a JSON file. Returns `Default` on any error.
fn load_groups_config(path: &str) -> GroupsConfig {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<GroupsConfig>(&content) {
            Ok(config) => config,
            Err(e) => {
                warn!(error = %e, path, "failed to parse groups config");
                GroupsConfig::default()
            }
        },
        Err(_) => GroupsConfig::default(),
    }
}

impl GroupMap {
    /// Load group mapping from groups.json for a specific channel.
    /// Builds a reverse lookup: provider chat_id → group slug.
    pub fn load(groups_file: &str, channel_name: &str) -> Self {
        let config = load_groups_config(groups_file);
        let map = Self::build_reverse_map(&config, channel_name);
        debug!(groups = map.len(), "loaded group mapping");
        Self { map }
    }

    fn build_reverse_map(config: &GroupsConfig, channel_name: &str) -> HashMap<String, String> {
        let mut reverse = HashMap::new();
        for (group, entry) in &config.groups {
            if let Some(mapping) = entry.channels.get(channel_name) {
                reverse.insert(mapping.chat_id.clone(), group.clone());
            }
        }
        reverse
    }

    /// Resolve a provider chat ID to a group slug.
    /// Returns `None` if the chat ID has no mapping.
    pub fn resolve(&self, chat_id: &str) -> Option<String> {
        self.map.get(chat_id).cloned()
    }

    /// Register a new chat_id → auto-generated slug. Returns the slug.
    /// `slug_prefix` is the channel type prefix (e.g. `"tg"`, `"slack"`).
    /// Does nothing if chat_id is already mapped.
    pub fn register(&mut self, chat_id: &str, slug_prefix: &str) -> String {
        if let Some(existing) = self.map.get(chat_id) {
            return existing.clone();
        }
        let slug = auto_group_slug(chat_id, slug_prefix);
        self.map.insert(chat_id.to_string(), slug.clone());
        info!(chat_id, slug, "registered new group");
        slug
    }

    /// Persist the current auto-registered mappings to `groups_d_file`.
    /// Reads existing file, updates entries for this channel, writes atomically.
    pub fn persist(&self, groups_d_file: &str, channel_name: &str) -> Result<()> {
        // Load existing groups.d file (may contain entries from previous runs)
        let mut config = load_groups_config(groups_d_file);

        // Update: for each chat_id→slug in our map, ensure the groups.d config has
        // a matching entry.
        for (chat_id, slug) in &self.map {
            let entry = config
                .groups
                .entry(slug.clone())
                .or_insert_with(|| GroupEntry {
                    trigger_pattern: None,
                    trigger_mode: TriggerMode::Mention,
                    channels: HashMap::new(),
                });
            entry.channels.insert(
                channel_name.to_string(),
                ChannelMapping {
                    chat_id: chat_id.clone(),
                },
            );
        }

        // Atomic write: write to tmp file, then rename
        let path = Path::new(groups_d_file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp_path = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(&config)?;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp_path, path)?;

        debug!(
            file = groups_d_file,
            groups = config.groups.len(),
            "persisted groups.d file"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_group() {
        let mut map = HashMap::new();
        map.insert("-1001234567890".to_string(), "family-chat".to_string());
        let gm = GroupMap { map };
        assert_eq!(gm.resolve("-1001234567890"), Some("family-chat".into()));
    }

    #[test]
    fn resolve_unmapped_returns_none() {
        let gm = GroupMap {
            map: HashMap::new(),
        };
        assert_eq!(gm.resolve("-1001234567890"), None);
        assert_eq!(gm.resolve("C0123456789"), None);
        assert_eq!(gm.resolve("1234567890@s.whatsapp.net"), None);
    }

    #[test]
    fn load_missing_file() {
        let gm = GroupMap::load("/nonexistent/groups.json", "test-channel");
        assert!(gm.map.is_empty());
    }

    #[test]
    fn load_valid_groups_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.json");
        std::fs::write(
            &path,
            r#"{
                "groups": {
                    "family-chat": {
                        "trigger_mode": "always",
                        "channels": {
                            "telegram-bot-1": { "chat_id": "-1001234567890" },
                            "slack-bot": { "chat_id": "C123" }
                        }
                    },
                    "work-team": {
                        "trigger_mode": "mention",
                        "channels": {
                            "telegram-bot-1": { "chat_id": "-1009876543210" }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let gm = GroupMap::load(path.to_str().unwrap(), "telegram-bot-1");
        assert_eq!(gm.resolve("-1001234567890"), Some("family-chat".into()));
        assert_eq!(gm.resolve("-1009876543210"), Some("work-team".into()));
        // Slack mapping not loaded for telegram channel
        assert_eq!(gm.resolve("C123"), None);
    }

    #[test]
    fn auto_slug_strips_negative() {
        assert_eq!(auto_group_slug("-1001234567890", "tg"), "tg-1001234567890");
        assert_eq!(auto_group_slug("1001234567890", "tg"), "tg-1001234567890");
    }

    #[test]
    fn auto_slug_slack_lowercases() {
        assert_eq!(auto_group_slug("C0123456789", "slack"), "slack-c0123456789");
    }

    #[test]
    fn register_new_chat() {
        let mut gm = GroupMap {
            map: HashMap::new(),
        };
        let slug = gm.register("-1001234567890", "tg");
        assert_eq!(slug, "tg-1001234567890");
        assert_eq!(
            gm.resolve("-1001234567890"),
            Some("tg-1001234567890".into())
        );
    }

    #[test]
    fn register_existing_noop() {
        let mut map = HashMap::new();
        map.insert("-1001234567890".to_string(), "family-chat".to_string());
        let mut gm = GroupMap { map };
        // Should return existing slug, not overwrite
        let slug = gm.register("-1001234567890", "tg");
        assert_eq!(slug, "family-chat");
    }

    #[test]
    fn persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let groups_d_file = dir.path().join("groups.d").join("test-channel.json");

        let mut gm = GroupMap {
            map: HashMap::new(),
        };
        gm.register("-1001234567890", "tg");
        gm.persist(groups_d_file.to_str().unwrap(), "test-channel")
            .unwrap();

        // Verify file was written
        assert!(groups_d_file.exists());

        // Reload and verify
        let config: GroupsConfig =
            serde_json::from_str(&std::fs::read_to_string(&groups_d_file).unwrap()).unwrap();
        assert!(config.groups.contains_key("tg-1001234567890"));
        let entry = &config.groups["tg-1001234567890"];
        assert_eq!(entry.trigger_mode, TriggerMode::Mention);
        assert_eq!(entry.channels["test-channel"].chat_id, "-1001234567890");
    }
}
