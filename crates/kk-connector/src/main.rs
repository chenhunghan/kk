use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tracing::{error, info};

use kk_connector::config::ConnectorConfig;
use kk_connector::groups::GroupMap;
use kk_connector::inbound::process_inbound;
use kk_connector::outbound::{poll_outbound, poll_stream};
use kk_connector::provider::ChatProvider;
use kk_connector::provider::ConnectorEvent;
use kk_connector::provider::discord::DiscordProvider;
use kk_connector::provider::github::GithubProvider;
use kk_connector::provider::slack::SlackProvider;
use kk_connector::provider::telegram::TelegramProvider;
use kk_connector::provider::whatsapp::WhatsappProvider;

#[tokio::main]
async fn main() -> Result<()> {
    kk_core::logging::init();

    let config = ConnectorConfig::from_env()?;
    info!(
        channel = config.channel_name,
        channel_type = config.channel_type,
        "starting kk-connector"
    );

    // Ensure queue directories
    std::fs::create_dir_all(&config.inbound_dir)?;
    std::fs::create_dir_all(&config.outbox_dir)?;
    std::fs::create_dir_all(&config.stream_dir)?;

    // Ensure groups.d parent directory exists
    if let Some(parent) = std::path::Path::new(&config.groups_d_file).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Load initial group mapping from groups.d/{channel}.json
    let group_map = GroupMap::load(&config.groups_d_file, &config.channel_name);

    // Channel for inbound events from provider dispatcher to processor
    let (inbound_tx, inbound_rx) = mpsc::channel::<ConnectorEvent>(256);

    // Initialize provider-specific inbound dispatcher and outbound sender
    let (inbound_handle, sender): (JoinHandle<()>, Box<dyn ChatProvider>) = match config
        .channel_type
        .as_str()
    {
        "telegram" => {
            let token = config
                .telegram_bot_token
                .as_deref()
                .context("TELEGRAM_BOT_TOKEN required for telegram channel type")?;

            let telegram = TelegramProvider::new(token).await?;
            let sender = Box::new(telegram.sender());
            info!(bot_username = telegram.bot_username(), "bot identity");

            let handle = tokio::spawn(async move {
                telegram.run_inbound(inbound_tx).await;
            });

            (handle, sender)
        }
        "slack" => {
            let bot_token = config
                .slack_bot_token
                .as_deref()
                .context("SLACK_BOT_TOKEN required for slack channel type")?;
            let app_token = config
                .slack_app_token
                .as_deref()
                .context("SLACK_APP_TOKEN required for slack channel type")?;

            let slack = SlackProvider::new(bot_token, app_token).await?;
            info!(bot_user_id = slack.bot_user_id(), "bot identity");
            let sender = Box::new(slack.sender());

            let handle = tokio::spawn(async move {
                slack.run_inbound(inbound_tx).await;
            });

            (handle, sender)
        }
        "discord" => {
            let token = config
                .discord_bot_token
                .as_deref()
                .context("DISCORD_BOT_TOKEN required for discord channel type")?;

            let discord = DiscordProvider::new(token).await?;
            info!(bot_user_id = discord.bot_user_id(), "bot identity");
            let sender = Box::new(discord.sender());

            let handle = tokio::spawn(async move {
                discord.run_inbound(inbound_tx).await;
            });

            (handle, sender)
        }
        "github" => {
            let token = config
                .github_token
                .as_deref()
                .context("GITHUB_TOKEN required for github channel type")?;

            let github = GithubProvider::new(
                token,
                config.github_webhook_port,
                config.github_webhook_secret.clone(),
            )
            .await?;
            info!(bot_login = github.bot_login(), "bot identity");
            let sender = Box::new(github.sender());

            let handle = tokio::spawn(async move {
                github.run_inbound(inbound_tx).await;
            });

            (handle, sender)
        }
        "whatsapp" => {
            let token = config
                .whatsapp_token
                .as_deref()
                .context("WHATSAPP_TOKEN required for whatsapp channel type")?;
            let phone_number_id = config
                .whatsapp_phone_number_id
                .as_deref()
                .context("WHATSAPP_PHONE_NUMBER_ID required for whatsapp channel type")?;
            let verify_token = config
                .whatsapp_webhook_verify_token
                .as_deref()
                .context("WHATSAPP_WEBHOOK_VERIFY_TOKEN required for whatsapp channel type")?;

            let whatsapp = WhatsappProvider::new(
                token,
                phone_number_id,
                config.whatsapp_webhook_port,
                verify_token,
            )
            .await?;
            info!(phone_number_id = whatsapp.phone_number_id(), "bot identity");
            let sender = Box::new(whatsapp.sender());

            let handle = tokio::spawn(async move {
                whatsapp.run_inbound(inbound_tx).await;
            });

            (handle, sender)
        }
        other => bail!(
            "unsupported channel type: {other} (supported: telegram, slack, discord, github, whatsapp)"
        ),
    };

    // Inbound processor: receives ConnectorEvent, normalizes, writes to /data/inbound/
    let inbound_config = config.clone();
    let processor_handle = tokio::spawn(async move {
        process_inbound(inbound_rx, &inbound_config, group_map).await;
    });

    // Outbound poller: polls /data/outbox/{channel}/, sends via provider
    let outbound_config = config.clone();
    let outbound_handle = tokio::spawn(async move {
        let interval = Duration::from_millis(outbound_config.outbound_poll_interval_ms);
        loop {
            if let Err(e) = poll_outbound(
                &outbound_config.outbox_dir,
                &outbound_config.channel_name,
                &*sender,
            )
            .await
            {
                error!(error = %e, "outbound poll error");
            }
            if let Err(e) = poll_stream(&outbound_config.stream_dir, &*sender).await {
                error!(error = %e, "stream poll error");
            }
            sleep(interval).await;
        }
    });

    info!("connector loops started");

    tokio::select! {
        res = inbound_handle => { error!("inbound dispatcher exited"); res?; }
        res = processor_handle => { error!("inbound processor exited"); res?; }
        res = outbound_handle => { error!("outbound poller exited"); res?; }
    }

    Ok(())
}
