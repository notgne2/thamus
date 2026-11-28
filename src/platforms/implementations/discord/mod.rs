use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serenity::all::{Client, Context, EventHandler, Message as DiscordMessage, Ready};
use serenity::http::Http;
use serenity::prelude::GatewayIntents;
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use tracing::{debug, error, info};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::config::{ConfigSnapshot, SharedConfig};
use crate::instance::InstanceProps;
use crate::platforms::{Platform, PlatformInstance};

pub mod instance;
pub use instance::DiscordPlatformInstance;

/// Configures the Discord platform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Token to connect with
    pub token: String,
    /// Override the display names for given usernames
    #[serde(default)]
    pub name_overrides: std::collections::HashMap<String, String>,
}

pub struct DiscordPlatform {
    config: SharedConfig,

    http: Arc<RwLock<Option<Arc<Http>>>>,
    bot_id: Arc<RwLock<Option<String>>>,

    instance_tx: broadcast::Sender<(Arc<dyn PlatformInstance>, InstanceProps)>,
    instances: Arc<RwLock<HashMap<String, Arc<DiscordPlatformInstance>>>>,
}

impl DiscordPlatform {
    pub fn new(config: SharedConfig) -> Self {
        Self {
            bot_id: Arc::new(RwLock::new(None)),
            instance_tx: broadcast::channel(100).0,
            instances: Arc::new(RwLock::new(HashMap::new())),
            config,
            http: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl Platform for DiscordPlatform {
    fn instance_receiver(&self) -> broadcast::Receiver<(Arc<dyn PlatformInstance>, InstanceProps)> {
        self.instance_tx.subscribe()
    }

    async fn run(&self, config_snapshot: &ConfigSnapshot) -> Result<!> {
        let Some(discord_config) = &config_snapshot.platforms.discord else {
            bail!("Discord disabled (why was this launched?)");
        };

        let http = Arc::new(Http::new(&discord_config.token));
        *self.http.write().await = Some(http.clone());

        let handler = DiscordPlatformHandler {
            config: self.config.clone(),
            http: http.clone(),
            bot_id: self.bot_id.clone(),
            instance_tx: self.instance_tx.clone(),
            instances: self.instances.clone(),
        };

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let mut client = Client::builder(&discord_config.token, intents)
            .event_handler(handler)
            .await
            .context("creating Discord client")?;

        client.start().await.context("Starting Discord client")?;

        Err(anyhow!("Discord client stopped unexpectedly"))
    }
}

struct DiscordPlatformHandler {
    config: SharedConfig,

    http: Arc<Http>,
    bot_id: Arc<RwLock<Option<String>>>,

    instance_tx: broadcast::Sender<(Arc<dyn PlatformInstance>, InstanceProps)>,
    instances: Arc<RwLock<HashMap<String, Arc<DiscordPlatformInstance>>>>,
}

#[async_trait]
impl EventHandler for DiscordPlatformHandler {
    async fn message(&self, ctx: Context, msg: DiscordMessage) {
        if let Some(bot_id) = self.bot_id.read().await.as_ref()
            && msg.author.id.to_string() == *bot_id
        {
            return;
        }

        if msg.author.bot && (msg.content.starts_with('✍') || msg.content.starts_with("[🤖")) {
            return;
        }

        let instance_id = format!("discord_{}", msg.channel_id);

        let mut instances = self.instances.write().await;
        let instance: Arc<DiscordPlatformInstance> =
            if let Some(instance) = instances.get(&instance_id) {
                let x = instance.clone();
                drop(instances);
                x
            } else {
                let (tx, rx) = broadcast::channel(100);
                let new_instance = Arc::new(DiscordPlatformInstance {
                    config: self.config.clone(),

                    channel_id: msg.channel_id,
                    http: self.http.clone(),
                    message_tx: tx,
                    message_rx: rx,
                    bot_id: self.bot_id.clone(),
                    incomplete_message: Arc::new(Mutex::new(None)),
                    pending_update: Arc::new(watch::channel(None).0),
                    inflight: Arc::new(Mutex::new(())),
                    working: Arc::new(Mutex::new(false)),
                    context: ctx.clone(),
                    last_status: Arc::new(Mutex::new(None)),
                });

                instances.insert(instance_id.clone(), new_instance.clone());
                drop(instances);

                if let Err(e) = self.instance_tx.send((
                    new_instance.clone(),
                    InstanceProps {
                        tools: true,
                        thinking: true,
                        voice: false,
                    },
                )) {
                    debug!("Failed to send new instance: {e}");
                }

                new_instance.run();

                new_instance
            };

        if let Err(e) = instance.handle_message(&ctx, &msg).await {
            error!("Failed to handle message in platform instance: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Discord bot logged in as {}", ready.user.name);
        let mut bot_id = self.bot_id.write().await;
        *bot_id = Some(ready.user.id.to_string());
    }
}
