use std::sync::Arc;

use futures::StreamExt;
use kube::{
    Api, Client,
    runtime::controller::{Action, Controller},
    runtime::watcher,
};
use tracing::{error, info};

use crate::crd::Skill;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

struct Ctx {
    #[allow(dead_code)]
    client: Client,
}

async fn reconcile(skill: Arc<Skill>, _ctx: Arc<Ctx>) -> Result<Action, Error> {
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    let source = &skill.spec.source;
    info!(skill = name, source = source.as_str(), "reconciling skill");

    // TODO: implement skill reconciliation
    // 1. Parse source (owner/repo/path)
    // 2. Update status → Fetching
    // 3. Git clone (shallow) to temp dir
    // 4. Validate skill dir + SKILL.md exist
    // 5. Parse YAML frontmatter
    // 6. Copy to PVC at /data/skills/{name}/
    // 7. Set permissions
    // 8. Update status → Ready

    Ok(Action::await_change())
}

fn error_policy(skill: Arc<Skill>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    let name = skill.metadata.name.as_deref().unwrap_or("unknown");
    error!(skill = name, error = %err, "skill reconciliation error");
    Action::requeue(std::time::Duration::from_secs(60))
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let skills: Api<Skill> = Api::default_namespaced(client.clone());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
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
