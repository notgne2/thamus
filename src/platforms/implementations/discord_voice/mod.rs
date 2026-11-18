use std::collections::HashMap;

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use serenity::all::{
    Client, Context, EventHandler, GuildId, Message as DiscordMessage, Ready, VoiceState,
};
use serenity::http::Http;
use serenity::prelude::GatewayIntents;
use songbird::{CoreEvent, SerenityInit as _};
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::{debug, error, info, warn};

use anyhow::{Context as _, Result, anyhow, bail};

use super::super::{Platform, PlatformInstance};

use crate::CommonResources;
use crate::config::{ConfigSnapshot, SharedConfig};
use crate::instance::InstanceProps;

pub mod instance;
pub use instance::DiscordVoicePlatformInstance;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordVoiceConfig {
    /// Token to connect with
    pub token: String,

    /// Override the display names for given usernames
    #[serde(default)]
    pub name_overrides: std::collections::HashMap<String, String>,
}

pub struct DiscordVoicePlatform {
    config: SharedConfig,
    serenity_http: Arc<RwLock<Option<Arc<Http>>>>,
    bot_id: Arc<RwLock<Option<String>>>,

    http: reqwest::Client,

    instance_tx: broadcast::Sender<(Arc<dyn PlatformInstance>, InstanceProps)>,
    instances: Arc<RwLock<HashMap<String, Arc<DiscordVoicePlatformInstance>>>>,

    joining: Arc<DashMap<GuildId, Arc<Mutex<()>>>>,
    registered_receivers: Arc<DashSet<GuildId>>,
}

impl DiscordVoicePlatform {
    pub fn new(common: &CommonResources) -> Self {
        Self {
            serenity_http: Arc::new(RwLock::new(None)),
            config: common.config.clone(),
            bot_id: Arc::new(RwLock::new(None)),
            http: common.http.clone(),
            instance_tx: broadcast::channel(100).0,
            instances: Arc::new(RwLock::new(HashMap::new())),
            joining: Arc::new(DashMap::new()),
            registered_receivers: Arc::new(DashSet::new()),
        }
    }
}

#[async_trait]
impl Platform for DiscordVoicePlatform {
    fn instance_receiver(
        &self,
    ) -> tokio::sync::broadcast::Receiver<(Arc<dyn PlatformInstance>, InstanceProps)> {
        self.instance_tx.subscribe()
    }

    async fn run(&self, config_snapshot: &ConfigSnapshot) -> Result<!> {
        let Some(discord_config) = &config_snapshot.platforms.discord else {
            bail!("Discord disabled (why was this launched?)");
        };

        let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;

        let http = Arc::new(Http::new(&discord_config.token));
        *self.serenity_http.write().await = Some(http.clone());

        let handler = DiscordVoicePlatformHandler {
            serenity_http: http.clone(),
            bot_id: self.bot_id.clone(),
            http: self.http.clone(),
            instance_tx: self.instance_tx.clone(),
            instances: self.instances.clone(),
            joining: self.joining.clone(),
            registered_receivers: self.registered_receivers.clone(),
            config: self.config.clone(),
        };

        let songbird_config = songbird::Config::default().decode_mode(
            songbird::driver::DecodeMode::Decode(songbird::driver::DecodeConfig::default()),
        );

        let mut client = Client::builder(&discord_config.token, intents)
            .event_handler(handler)
            .register_songbird_from_config(songbird_config)
            .await
            .context("creating Discord voice client")?;

        client.start().await.context("Starting Discord client")?;

        Err(anyhow!("Discord client stopped unexpectedly"))
    }
}

pub struct DiscordVoicePlatformHandler {
    serenity_http: Arc<Http>,
    bot_id: Arc<RwLock<Option<String>>>,

    http: reqwest::Client,

    instance_tx: broadcast::Sender<(Arc<dyn PlatformInstance>, InstanceProps)>,
    instances: Arc<RwLock<HashMap<String, Arc<DiscordVoicePlatformInstance>>>>,

    joining: Arc<DashMap<GuildId, Arc<Mutex<()>>>>,
    registered_receivers: Arc<DashSet<GuildId>>,
    config: SharedConfig,
}

#[async_trait]
impl EventHandler for DiscordVoicePlatformHandler {
    async fn message(&self, _ctx: Context, _msg: DiscordMessage) {}

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Discord VOICE bot logged in as {}", ready.user.name);
        let mut bot_id = self.bot_id.write().await;
        *bot_id = Some(ready.user.id.to_string());
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        debug!("Got voice state update!");

        let Some(channel_id) = new.channel_id else {
            warn!("voice_state_update: new.channel_id is None");
            return;
        };

        let Some(guild_id) = new.guild_id else {
            warn!("voice_state_update: new.guild_id is None");
            return;
        };

        let Some(manager) = songbird::get(&ctx).await else {
            warn!("Missing songbird manager placed at initialization");
            return;
        };

        // Use a per-guild join lock to avoid duplicate joins.
        let join_lock = self
            .joining
            .entry(guild_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _join_guard = join_lock.lock().await;

        // Look up or create the instance for this guild+channel.
        let instance_id = format!("discord_voice_{channel_id}");

        let instance = {
            let mut instances = self.instances.write().await;

            if let Some(existing) = instances.get(&instance_id) {
                existing.clone()
            } else {
                let (tx, rx) = broadcast::channel(100);

                let new_instance = Arc::new(DiscordVoicePlatformInstance {
                    config: self.config.clone(),
                    channel_id,
                    guild_id,
                    serenity_http: self.serenity_http.clone(),
                    http: self.http.clone(),
                    message_tx: tx,
                    message_rx: rx,
                    bot_id: self.bot_id.clone(),
                    context: ctx.clone(),
                    outputted_pos: Mutex::new(0),
                    working_track: Mutex::new(None),
                });

                instances.insert(instance_id.clone(), new_instance.clone());
                drop(instances);

                if let Err(e) = self.instance_tx.send((
                    new_instance.clone(),
                    InstanceProps {
                        tools: true,
                        thinking: true,
                        voice: true,
                    },
                )) {
                    debug!("Failed to send new voice instance: {e}");
                }

                new_instance
            }
        };

        // Set up the audio Receiver once per guild. Re-registering here on every
        // voice_state_update causes every VoiceTick to be processed multiple times.
        if self.registered_receivers.insert(guild_id) {
            let call = manager.get_or_insert(guild_id);
            let mut locked_call = call.lock().await;

            let evt_receiver = instance.receiver();

            locked_call
                .add_global_event(CoreEvent::SpeakingStateUpdate.into(), evt_receiver.clone());
            locked_call.add_global_event(CoreEvent::VoiceTick.into(), evt_receiver);
        }

        if let Err(e) = manager.join(guild_id, channel_id).await {
            error!("Failed to join voice channel: {}", e);
        }
    }
}
