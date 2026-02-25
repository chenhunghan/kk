use std::sync::Arc;

use futures::StreamExt;
use kube::{
    Api, Client,
    runtime::controller::{Action, Controller},
    runtime::watcher,
};
use tracing::{error, info};

use crate::crd::Channel;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
}

struct Ctx {
    #[allow(dead_code)]
    client: Client,
}

async fn reconcile(channel: Arc<Channel>, _ctx: Arc<Ctx>) -> Result<Action, Error> {
    let name = channel.metadata.name.as_deref().unwrap_or("unknown");
    let ns = channel.metadata.namespace.as_deref().unwrap_or("default");
    info!(channel = name, namespace = ns, "reconciling channel");

    // TODO: implement channel reconciliation
    // 1. Validate secretRef exists
    // 2. Build desired Connector Deployment
    // 3. Create or update Deployment
    // 4. Ensure outbox directory exists
    // 5. Update Channel status

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

fn error_policy(channel: Arc<Channel>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    let name = channel.metadata.name.as_deref().unwrap_or("unknown");
    error!(channel = name, error = %err, "channel reconciliation error");
    Action::requeue(std::time::Duration::from_secs(30))
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let channels: Api<Channel> = Api::default_namespaced(client.clone());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });

    info!("starting channel controller");

    Controller::new(channels, watcher::Config::default())
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
