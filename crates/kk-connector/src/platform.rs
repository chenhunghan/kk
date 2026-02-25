//! Platform abstraction for messaging connectors.

use anyhow::Result;
use kk_core::types::OutboundMessage;

/// Trait for platform-specific messaging implementations.
#[allow(async_fn_in_trait, dead_code)]
pub trait Platform: Send + Sync {
    /// Send a message via the platform API.
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;

    /// Start listening for inbound messages. Calls the callback for each.
    async fn listen<F>(&self, on_message: F) -> Result<()>
    where
        F: Fn(InboundRaw) + Send + Sync + 'static;
}

/// Raw inbound data from a platform, before normalization.
#[derive(Debug)]
#[allow(dead_code)]
pub struct InboundRaw {
    pub platform_group_id: String,
    pub sender_name: String,
    pub text: String,
    pub timestamp: u64,
    pub is_bot: bool,
    pub platform_meta: serde_json::Value,
}

// TODO: implement platform-specific clients
// pub mod telegram;
// pub mod slack;
// pub mod discord;
