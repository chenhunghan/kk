mod common;

common::provider_e2e_tests! {
    channel_type: "telegram",
    chat_id: "-1001234567890",
    expected_channel_type: kk_core::types::ChannelType::Telegram,
    expected_auto_slug: "tg-1001234567890",
    meta_chat_id_key: "chat_id",
    dummy_sender: || {
        kk_connector::provider::telegram::TelegramOutbound::new(
            teloxide::Bot::new("0:fake-token-for-test"),
        )
    },
}
