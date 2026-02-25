use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kk_core::paths::DataPaths;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{self, Event as Finalizer};
use kube::runtime::watcher;
use serde::Deserialize;
use tracing::{error, info};

use crate::config::ControllerConfig;
use crate::crd::Skill;

const FINALIZER_NAME: &str = "skills.kk.io/cleanup";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid source: {0}")]
    InvalidSource(String),
    #[error("skill validation: {0}")]
    Validation(String),
    #[error("git clone failed: {0}")]
    GitClone(String),
    #[error("finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Error>>),
}

pub struct Ctx {
    pub client: Client,
    pub config: ControllerConfig,
}

/// Parsed GitHub source path: owner/repo/path/to/skill
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    pub owner: String,
    pub repo: String,
    pub path: String,
    pub clone_url: String,
}

/// Frontmatter extracted from SKILL.md
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
}

/// Parse a GitHub source string like "owner/repo/path/to/skill".
/// Requires at least 3 segments (owner, repo, and at least one path component).
pub fn parse_source(source: &str) -> Result<ParsedSource, Error> {
    let parts: Vec<&str> = source.splitn(3, '/').collect();
    if parts.len() < 3 {
        return Err(Error::InvalidSource(format!(
            "expected owner/repo/path, got '{source}'"
        )));
    }

    let owner = parts[0];
    let repo = parts[1];
    let path = parts[2];

    if owner.is_empty() || repo.is_empty() || path.is_empty() {
        return Err(Error::InvalidSource(format!(
            "empty segment in source '{source}'"
        )));
    }

    Ok(ParsedSource {
        owner: owner.into(),
        repo: repo.into(),
        path: path.into(),
        clone_url: format!("https://github.com/{owner}/{repo}.git"),
    })
}

/// Parse YAML frontmatter from SKILL.md content.
/// Expects content starting with `---` delimiter, YAML block, then `---`.
pub fn parse_frontmatter(content: &str) -> Result<SkillFrontmatter, Error> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(Error::Validation(
            "SKILL.md must start with --- frontmatter delimiter".into(),
        ));
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let end = after_first.find("---").ok_or_else(|| {
        Error::Validation("SKILL.md missing closing --- frontmatter delimiter".into())
    })?;

    let yaml_str = &after_first[..end];
    let fm: SkillFrontmatter = serde_yaml::from_str(yaml_str)
        .map_err(|e| Error::Validation(format!("invalid frontmatter YAML: {e}")))?;

    if fm.name.is_empty() {
        return Err(Error::Validation("frontmatter 'name' is required".into()));
    }
    if fm.description.is_empty() {
        return Err(Error::Validation(
            "frontmatter 'description' is required".into(),
        ));
    }

    Ok(fm)
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Set permissions: readable by all, scripts executable.
fn set_skill_permissions(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            // Directories: rwxr-xr-x
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            set_skill_permissions(&path)?;
        } else {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".sh") || name_str.ends_with(".py") || name_str == "run" {
                // Scripts: rwxr-xr-x
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            } else {
                // Regular files: rw-r--r--
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            }
        }
    }
    Ok(())
}

async fn update_skill_status(
    client: &Client,
    skill: &Skill,
    phase: &str,
    message: &str,
    skill_name: &str,
    skill_description: &str,
    last_fetched: &str,
) -> Result<(), Error> {
    let ns = skill.metadata.namespace.as_deref().unwrap_or("default");
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    let skills: Api<Skill> = Api::namespaced(client.clone(), ns);

    let status = serde_json::json!({
        "status": {
            "phase": phase,
            "message": message,
            "skillName": skill_name,
            "skillDescription": skill_description,
            "lastFetched": last_fetched,
        }
    });

    skills
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&status))
        .await?;

    Ok(())
}

async fn apply_skill(skill: Arc<Skill>, ctx: &Ctx) -> Result<Action, Error> {
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    let source = &skill.spec.source;
    info!(skill = name, source = source.as_str(), "applying skill");

    // 1. Parse source
    let parsed = match parse_source(source) {
        Ok(p) => p,
        Err(e) => {
            update_skill_status(&ctx.client, &skill, "Error", &e.to_string(), "", "", "").await?;
            return Ok(Action::requeue(Duration::from_secs(300)));
        }
    };

    // 2. Set status → Fetching
    update_skill_status(
        &ctx.client,
        &skill,
        "Fetching",
        "Cloning repository",
        "",
        "",
        "",
    )
    .await?;

    // 3. Git clone (shallow) to temp dir
    let tmp_dir = std::env::temp_dir().join(format!("kk-skill-{name}"));
    // Clean up any previous clone attempt
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let clone_result = tokio::time::timeout(
        Duration::from_secs(ctx.config.skill_clone_timeout),
        tokio::process::Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--single-branch",
                "--filter=blob:none",
                &parsed.clone_url,
                &tmp_dir.to_string_lossy(),
            ])
            .output(),
    )
    .await;

    match clone_result {
        Ok(Ok(output)) if output.status.success() => {}
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = format!("git clone failed: {stderr}");
            let _ = std::fs::remove_dir_all(&tmp_dir);
            update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
            return Err(Error::GitClone(msg));
        }
        Ok(Err(e)) => {
            let msg = format!("git clone command error: {e}");
            let _ = std::fs::remove_dir_all(&tmp_dir);
            update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
            return Err(Error::GitClone(msg));
        }
        Err(_) => {
            let msg = format!(
                "git clone timed out after {}s",
                ctx.config.skill_clone_timeout
            );
            let _ = std::fs::remove_dir_all(&tmp_dir);
            update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
            return Err(Error::GitClone(msg));
        }
    };

    // 4. Validate skill path exists in cloned repo
    let skill_src = tmp_dir.join(&parsed.path);
    if !skill_src.is_dir() {
        let msg = format!("skill path '{}' not found in repo", parsed.path);
        let _ = std::fs::remove_dir_all(&tmp_dir);
        update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
        return Err(Error::Validation(msg));
    }

    // 5. Validate SKILL.md exists and parse frontmatter
    let skill_md_path = skill_src.join("SKILL.md");
    if !skill_md_path.is_file() {
        let msg = format!("SKILL.md not found in {}", parsed.path);
        let _ = std::fs::remove_dir_all(&tmp_dir);
        update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
        return Err(Error::Validation(msg));
    }

    let skill_md_content = std::fs::read_to_string(&skill_md_path)?;
    let frontmatter = match parse_frontmatter(&skill_md_content) {
        Ok(fm) => fm,
        Err(e) => {
            let msg = e.to_string();
            let _ = std::fs::remove_dir_all(&tmp_dir);
            update_skill_status(&ctx.client, &skill, "Error", &msg, "", "", "").await?;
            return Err(e);
        }
    };

    // 6. Copy skill dir to /data/skills/{skill-name}/
    let data_paths = DataPaths::new(&ctx.config.data_dir);
    let dest = data_paths.skill_dir(name);
    // Remove old version
    let _ = std::fs::remove_dir_all(&dest);
    copy_dir_recursive(&skill_src, &dest)?;

    // 7. Set permissions
    set_skill_permissions(&dest)?;

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // 8. Set status → Ready
    let now = chrono::Utc::now().to_rfc3339();
    update_skill_status(
        &ctx.client,
        &skill,
        "Ready",
        "Skill installed",
        &frontmatter.name,
        &frontmatter.description,
        &now,
    )
    .await?;

    info!(skill = name, "skill installed successfully");
    Ok(Action::await_change())
}

async fn cleanup_skill(skill: Arc<Skill>, ctx: &Ctx) -> Result<Action, Error> {
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    info!(skill = name, "cleaning up skill");

    let data_paths = DataPaths::new(&ctx.config.data_dir);
    let skill_dir = data_paths.skill_dir(name);
    if skill_dir.exists() {
        std::fs::remove_dir_all(&skill_dir)?;
        info!(skill = name, "removed skill directory");
    }

    Ok(Action::await_change())
}

pub async fn reconcile(skill: Arc<Skill>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = skill.metadata.namespace.as_deref().unwrap_or("default");
    let skills: Api<Skill> = Api::namespaced(ctx.client.clone(), ns);

    finalizer::finalizer(&skills, FINALIZER_NAME, skill, |event| async {
        match event {
            Finalizer::Apply(s) => apply_skill(s, &ctx).await,
            Finalizer::Cleanup(s) => cleanup_skill(s, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

fn error_policy(skill: Arc<Skill>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    error!(skill = name, error = %err, "skill reconciliation error");
    Action::requeue(std::time::Duration::from_secs(60))
}

pub async fn run(client: Client, config: ControllerConfig) -> anyhow::Result<()> {
    let ns = config.namespace.clone();
    let skills: Api<Skill> = Api::namespaced(client.clone(), &ns);
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        config,
    });

    info!("starting skill controller");

    Controller::new(skills, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => error!(error = %e, "skill controller error"),
            }
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_source_valid() {
        let result = parse_source("acme/skills-repo/tools/weather").unwrap();
        assert_eq!(result.owner, "acme");
        assert_eq!(result.repo, "skills-repo");
        assert_eq!(result.path, "tools/weather");
        assert_eq!(result.clone_url, "https://github.com/acme/skills-repo.git");
    }

    #[test]
    fn test_parse_source_minimal() {
        let result = parse_source("owner/repo/skill").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert_eq!(result.path, "skill");
    }

    #[test]
    fn test_parse_source_too_few_parts() {
        assert!(parse_source("owner/repo").is_err());
        assert!(parse_source("onlyone").is_err());
        assert!(parse_source("").is_err());
    }

    #[test]
    fn test_parse_source_empty_segment() {
        assert!(parse_source("/repo/path").is_err());
        assert!(parse_source("owner//path").is_err());
        assert!(parse_source("owner/repo/").is_err());
    }

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = r#"---
name: weather-lookup
description: Look up current weather for a city
---
# Weather Lookup

Use this skill to check the weather.
"#;
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "weather-lookup");
        assert_eq!(fm.description, "Look up current weather for a city");
    }

    #[test]
    fn test_parse_frontmatter_missing_name() {
        let content = r#"---
description: Some description
---
"#;
        // serde_yaml will error on missing required field
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn test_parse_frontmatter_no_delimiters() {
        let content = "# Just a markdown file\nNo frontmatter here.";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn test_parse_frontmatter_missing_closing() {
        let content = "---\nname: test\ndescription: test\n";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn test_parse_frontmatter_empty_name() {
        let content = r#"---
name: ""
description: Some description
---
"#;
        assert!(parse_frontmatter(content).is_err());
    }
}
