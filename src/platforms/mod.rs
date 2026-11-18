//! Platform abstraction layer
//!
//! Provides a provider-agnostic interface for platform implementations.
//! All platforms (Discord, Minecraft, etc.) implement the `Platform` trait.

use std::{path::Path, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    config::ConfigSnapshot,
    instance::{DreamState, InstanceProps},
    messages::{self},
    providers::llm,
    utils::tooling::BotTool,
};

pub mod implementations;

/// Configures platforms used
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformsConfig {
    /// Configure the Discord (chat) platform
    pub discord: Option<implementations::discord::DiscordConfig>,
    /// Configure the Discord voice platform
    pub discord_voice: Option<implementations::discord_voice::DiscordVoiceConfig>,
}

#[async_trait]
pub trait Platform: Send + Sync {
    async fn run(&self, config_snapshot: &ConfigSnapshot) -> Result<!>;

    fn instance_receiver(
        &self,
    ) -> tokio::sync::broadcast::Receiver<(Arc<dyn PlatformInstance>, InstanceProps)>;
}

pub enum PartialUpdate<'a, 'b> {
    Real(&'a messages::BotEntryKind, Option<&'b llm::openai::Usage>),
    // u16 bcus lol wot if over 100%
    Dream(bool, DreamState, u16),
}

#[async_trait]
pub trait PlatformInstance: Send + Sync {
    fn message_receiver(&self) -> tokio::sync::broadcast::Receiver<messages::PlatformMessage>;

    async fn set_working(&self, is_working: bool) -> Result<()>;

    async fn get_username(&self) -> Result<String>;

    async fn set_username(&self, username: &str) -> Result<()>;

    async fn send_partial<'a, 'b>(&self, partial_update: PartialUpdate<'a, 'b>) -> Result<()>;

    async fn send_final(
        &self,
        m_message: Result<(&messages::BotMessage, Option<&llm::openai::Usage>, bool), &str>,
    ) -> Result<Vec<messages::PlatformMessage>>;

    async fn set_status(&self, status: &str) -> Result<()>;

    async fn set_avatar(&self, avatar: &Path) -> Result<()>;

    async fn chat_info(&self) -> Result<String>;

    fn instance_id(&self) -> String;

    async fn platform_tools(&self) -> Result<Vec<Box<dyn BotTool>>> {
        Ok(vec![])
    }
}
