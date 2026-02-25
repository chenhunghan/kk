mod common;

common::provider_e2e_tests! {
    channel_type: "slack",
    chat_id: "C0123456789",
    expected_channel_type: kk_core::types::ChannelType::Slack,
    expected_auto_slug: "slack-c0123456789",
    meta_chat_id_key: "channel_id",
    dummy_sender: || {
        kk_connector::provider::ProviderSender::Slack(
            kk_connector::provider::slack::SlackSender::new(
                "xoxb-fake-token".into(),
                reqwest::Client::new(),
            ),
        )
    },
}
