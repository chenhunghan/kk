//! File path conventions for the shared PVC (Protocol §5)

use std::path::PathBuf;

/// All PVC paths relative to a data root directory.
#[derive(Debug, Clone)]
pub struct DataPaths {
    root: PathBuf,
}

impl DataPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    // --- Queue directories ---

    /// /data/inbound/ — Connectors write, Gateway reads
    pub fn inbound_dir(&self) -> PathBuf {
        self.root.join("inbound")
    }

    /// /data/groups/{group}/ — Gateway writes, Agent Jobs read
    pub fn group_queue_dir(&self, group: &str) -> PathBuf {
        self.root.join("groups").join(group)
    }

    /// Thread-aware group queue directory.
    /// - `None` → `/data/groups/{group}/`
    /// - `Some("42")` → `/data/groups/{group}/threads/42/`
    pub fn group_queue_dir_threaded(&self, group: &str, thread_id: Option<&str>) -> PathBuf {
        let base = self.group_queue_dir(group);
        match thread_id {
            Some(tid) => base.join("threads").join(tid),
            None => base,
        }
    }

    /// /data/outbox/{channel}/ — Gateway writes, Connectors read
    pub fn outbox_dir(&self, channel: &str) -> PathBuf {
        self.root.join("outbox").join(channel)
    }

    // --- Results ---

    /// /data/results/{session-id}/
    pub fn results_dir(&self, session_id: &str) -> PathBuf {
        self.root.join("results").join(session_id)
    }

    /// /data/results/{session-id}/request.json
    pub fn request_manifest(&self, session_id: &str) -> PathBuf {
        self.results_dir(session_id).join("request.json")
    }

    /// /data/results/{session-id}/status
    pub fn result_status(&self, session_id: &str) -> PathBuf {
        self.results_dir(session_id).join("status")
    }

    /// /data/results/{session-id}/response.jsonl
    pub fn result_response(&self, session_id: &str) -> PathBuf {
        self.results_dir(session_id).join("response.jsonl")
    }

    /// /data/results/.done/
    pub fn results_done_dir(&self) -> PathBuf {
        self.root.join("results").join(".done")
    }

    // --- Skills ---

    /// /data/skills/
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// /data/skills/{skill-name}/
    pub fn skill_dir(&self, name: &str) -> PathBuf {
        self.skills_dir().join(name)
    }

    // --- Sessions ---

    /// /data/sessions/{group}/
    pub fn session_dir(&self, group: &str) -> PathBuf {
        self.root.join("sessions").join(group)
    }

    /// Thread-aware session directory.
    /// - `None` → `/data/sessions/{group}/`
    /// - `Some("42")` → `/data/sessions/{group}/threads/42/`
    pub fn session_dir_threaded(&self, group: &str, thread_id: Option<&str>) -> PathBuf {
        let base = self.session_dir(group);
        match thread_id {
            Some(tid) => base.join("threads").join(tid),
            None => base,
        }
    }

    // --- Memory ---

    /// /data/memory/SOUL.md
    pub fn soul_md(&self) -> PathBuf {
        self.root.join("memory").join("SOUL.md")
    }

    /// /data/memory/{group}/CLAUDE.md
    pub fn group_claude_md(&self, group: &str) -> PathBuf {
        self.root.join("memory").join(group).join("CLAUDE.md")
    }

    // --- State ---

    /// /data/state/groups.json
    pub fn groups_json(&self) -> PathBuf {
        self.root.join("state").join("groups.json")
    }

    /// /data/state/groups.d/
    pub fn groups_d_dir(&self) -> PathBuf {
        self.root.join("state").join("groups.d")
    }

    /// /data/state/cursors.json
    pub fn cursors_json(&self) -> PathBuf {
        self.root.join("state").join("cursors.json")
    }

    /// Ensure all top-level directories exist.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        let dirs = [
            self.root.join("inbound"),
            self.root.join("outbox"),
            self.root.join("groups"),
            self.root.join("results"),
            self.results_done_dir(),
            self.root.join("skills"),
            self.root.join("sessions"),
            self.root.join("memory"),
            self.root.join("state"),
            self.root.join("state").join("groups.d"),
        ];
        for dir in &dirs {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Build a session ID from group slug and timestamp (Protocol §5.1)
pub fn session_id(group: &str, timestamp: u64) -> String {
    format!("{group}-{timestamp}")
}

/// Thread-aware session ID (Protocol §5.1).
/// - `None` → `{group}-{timestamp}`
/// - `Some("42")` → `{group}-t42-{timestamp}`
pub fn session_id_threaded(group: &str, thread_id: Option<&str>, timestamp: u64) -> String {
    match thread_id {
        Some(tid) => format!("{group}-t{tid}-{timestamp}"),
        None => session_id(group, timestamp),
    }
}

/// Build a K8s Job name from session ID
pub fn job_name(session_id: &str) -> String {
    sanitize_k8s_name(&format!("agent-{session_id}"))
}

/// Sanitize a string for use as a K8s resource name (max 63 chars, lowercase, alphanumeric + hyphens)
pub fn sanitize_k8s_name(name: &str) -> String {
    let mut result: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse multiple hyphens
    while result.contains("--") {
        result = result.replace("--", "-");
    }

    // Trim leading/trailing hyphens
    result = result.trim_matches('-').to_string();

    // Truncate to 63 chars
    if result.len() > 63 {
        result.truncate(54);
        result = result.trim_end_matches('-').to_string();
        // Append a short hash for uniqueness
        let hash = simple_hash(name);
        result = format!("{result}-{hash:08x}");
    }

    result
}

fn simple_hash(s: &str) -> u32 {
    s.bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id() {
        assert_eq!(
            session_id("family-chat", 1708801290),
            "family-chat-1708801290"
        );
    }

    #[test]
    fn test_job_name() {
        assert_eq!(
            job_name("family-chat-1708801290"),
            "agent-family-chat-1708801290"
        );
    }

    #[test]
    fn test_sanitize_k8s_name() {
        assert_eq!(sanitize_k8s_name("Hello_World!"), "hello-world");
        assert_eq!(sanitize_k8s_name("--foo--bar--"), "foo-bar");
    }

    #[test]
    fn test_sanitize_long_name() {
        let long = "a".repeat(100);
        let result = sanitize_k8s_name(&long);
        assert!(result.len() <= 63);
    }

    #[test]
    fn test_data_paths() {
        let paths = DataPaths::new("/data");
        assert_eq!(paths.inbound_dir(), PathBuf::from("/data/inbound"));
        assert_eq!(
            paths.group_queue_dir("family-chat"),
            PathBuf::from("/data/groups/family-chat")
        );
        assert_eq!(
            paths.outbox_dir("telegram-bot-1"),
            PathBuf::from("/data/outbox/telegram-bot-1")
        );
        assert_eq!(
            paths.result_status("family-chat-1234"),
            PathBuf::from("/data/results/family-chat-1234/status")
        );
    }

    #[test]
    fn test_group_queue_dir_threaded() {
        let paths = DataPaths::new("/data");
        assert_eq!(
            paths.group_queue_dir_threaded("dev-team", None),
            PathBuf::from("/data/groups/dev-team")
        );
        assert_eq!(
            paths.group_queue_dir_threaded("dev-team", Some("42")),
            PathBuf::from("/data/groups/dev-team/threads/42")
        );
    }

    #[test]
    fn test_session_dir_threaded() {
        let paths = DataPaths::new("/data");
        assert_eq!(
            paths.session_dir_threaded("dev-team", None),
            PathBuf::from("/data/sessions/dev-team")
        );
        assert_eq!(
            paths.session_dir_threaded("dev-team", Some("42")),
            PathBuf::from("/data/sessions/dev-team/threads/42")
        );
    }

    #[test]
    fn test_session_id_threaded() {
        assert_eq!(
            session_id_threaded("dev-team", None, 1708801290),
            "dev-team-1708801290"
        );
        assert_eq!(
            session_id_threaded("dev-team", Some("42"), 1708801290),
            "dev-team-t42-1708801290"
        );
        // Slack-style string thread ID
        assert_eq!(
            session_id_threaded("eng", Some("1708801285.000050"), 1708801290),
            "eng-t1708801285.000050-1708801290"
        );
    }
}
