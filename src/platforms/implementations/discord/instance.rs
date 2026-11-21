use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serenity::all::{
    ActivityData, ChannelId, Context, CreateAttachment, CreateMessage, EditProfile, OnlineStatus,
};
use serenity::http::Http;
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use anyhow::{Context as _, Result, bail};

use crate::config::{SharedConfig, snapshot_config};
use crate::instance::DreamState;
use crate::messages::{self};
use crate::platforms::{PartialUpdate, PlatformInstance};
use crate::providers::llm;
use crate::utils::{self, spawn_named};

/// Discord platform implementation for a specific channel
pub struct DiscordPlatformInstance {
    pub config: SharedConfig,

    pub channel_id: ChannelId,
    pub http: Arc<Http>,
    pub message_tx: broadcast::Sender<messages::PlatformMessage>,
    pub message_rx: broadcast::Receiver<messages::PlatformMessage>,
    pub bot_id: Arc<RwLock<Option<String>>>,
    /// Tracks the incomplete (non-final) message being streamed
    pub incomplete_message: Arc<Mutex<Option<(serenity::all::MessageId, String)>>>,
    /// Latest queued update text; `None` means no update pending.
    /// Senders always overwrite; the driver always sees the freshest value.
    pub pending_update: Arc<watch::Sender<Option<String>>>,
    /// Held (as a unit lock) by whichever task is currently the send driver.
    pub inflight: Arc<Mutex<()>>,
    pub context: Context,

    pub last_status: Arc<Mutex<Option<String>>>,

    pub working: Arc<Mutex<bool>>,
}

fn format_name(
    username: &str,
    display_name: Option<&str>,
    name_overrides: &std::collections::HashMap<String, String>,
) -> String {
    if let Some(over) = name_overrides.get(username) {
        return format!("@{over}");
    }

    if let Some(display_name) = display_name
        && !display_name.is_empty()
        && display_name != username
    {
        format!("@{username} (\"{display_name}\")")
    } else {
        format!("@{username}")
    }
}

impl DiscordPlatformInstance {
    pub async fn handle_message(&self, ctx: &Context, msg: &serenity::all::Message) -> Result<()> {
        let config_snapshot = crate::config::snapshot_config(&self.config).await;

        let Some(discord_config) = &config_snapshot.platforms.discord else {
            bail!("Discord disabled");
        };

        let name_overrides = &discord_config.name_overrides;

        let author_id = messages::PlatformUserId(msg.author.id.to_string());

        let mut mentions: Vec<messages::Mention> = Vec::new();

        let mut text = msg.content.clone();

        if let Some(ref reference) = msg.message_reference
            && let Some(message_id) = reference.message_id
        {
            match self
                .http
                .get_message(reference.channel_id, message_id)
                .await
            {
                Ok(real_ref) => {
                    let ref_author = real_ref.author;

                    let ref_display_name = if let Some(guild_id) = msg.guild_id
                        && let Ok(member) = guild_id.member(&ctx.http, ref_author.id).await
                    {
                        member.display_name().to_string()
                    } else {
                        ref_author.display_name().to_string()
                    };

                    let _ = std::fmt::Write::write_fmt(
                        &mut text,
                        format_args!(
                            "\n\n[in direct reply to a message by {}]",
                            format_name(&ref_author.name, Some(&ref_display_name), name_overrides)
                        ),
                    );
                }
                Err(err) => {
                    error!("Failed to get referenced message: {err:?}");
                    return Ok(());
                }
            }
        }

        for mention in &msg.mentions {
            let mention_id = messages::PlatformUserId(mention.id.to_string());
            let mention_name = mention.name.clone();
            mentions.push(messages::Mention {
                id: mention_id,
                name: mention_name,
            });

            let mention_display_name = if let Some(guild_id) = msg.guild_id
                && let Ok(member) = guild_id.member(&ctx.http, mention.id).await
            {
                member.display_name().to_string()
            } else {
                mention.display_name().to_string()
            };

            let name = format_name(&mention.name, Some(&mention_display_name), name_overrides);

            text = text.replace(&format!("<@{}>", mention.id), &name);
        }

        let author_display_name = if let Some(guild_id) = msg.guild_id
            && let Ok(member) = guild_id.member(&ctx.http, msg.author.id).await
        {
            member.display_name().to_string()
        } else {
            msg.author.display_name().to_string()
        };

        let mut attachments = vec![];

        for attachment in &msg.attachments {
            attachments.push(messages::ReceivedAttachment {
                source: messages::ReceivedAttachmentSource::Url(attachment.url.clone()),
                ext: PathBuf::from(attachment.filename.clone())
                    .extension()
                    .unwrap_or_else(|| OsStr::new("idk"))
                    .to_string_lossy()
                    .to_string(),
            });
        }

        let platform_msg = messages::PlatformMessage {
            id: msg.id.to_string(),
            author_id,
            author_name: format_name(&msg.author.name, Some(&author_display_name), name_overrides),
            text,
            mentions,
            attachments,
        };

        if let Err(e) = self.message_tx.send(platform_msg) {
            debug!("Failed to broadcast message: {e}");
        }

        Ok(())
    }

    pub fn run(&self) {
        let working = self.working.clone();
        let channel_id = self.channel_id;
        let http = self.http.clone();

        spawn_named("discord_broadcast_typing", async move {
            loop {
                sleep(Duration::from_secs(6)).await;
                if *working.lock().await
                    && let Err(e) = http.broadcast_typing(channel_id).await
                {
                    warn!("Error broadcasting ongoing typing: {e:?}");
                }
            }
        });
    }

    async fn dream_status_update(&self, dream_state: DreamState, pct: u16) -> Result<()> {
        if !pct.is_multiple_of(10) && pct != 0 && pct != 1 && pct != 99 && pct != 100 {
            return Ok(());
        }

        let s = format!("{dream_state} dream {pct}%");

        self.set_status(&s).await?;

        Ok(())
    }

    async fn send_text_update(&self, text: &str) -> Result<()> {
        // Always publish the latest text. If a driver is already running it
        // will pick this up; if not, we become the driver below.
        self.pending_update.send_replace(Some(text.to_string()));

        // Try to become the driver. If another task already holds `inflight`,
        // our value is already in the watch channel — we're done.
        let Ok(_inflight_guard) = self.inflight.try_lock() else {
            return Ok(());
        };

        // We are the driver. Drain the watch channel until no more updates
        // arrive, sending/editing the Discord message each iteration.
        let mut incomplete = self.incomplete_message.lock().await;

        // Atomically take the queued value; if none, we're done.
        while let Some(current_text) = self.pending_update.send_replace(None) {
            let text: &str = if current_text.len() >= 2000 {
                "(too long to update, please wait for final message)"
            } else {
                &current_text
            };

            if let Some((incomplete_id, incomplete_text)) = &*incomplete {
                if incomplete_text != text {
                    if text.is_empty() {
                        self.channel_id
                            .delete_message(&self.http, incomplete_id)
                            .await
                            .context("deleting message on Discord")?;
                        *incomplete = None;
                    } else {
                        let edited = self
                            .channel_id
                            .edit_message(
                                &self.http,
                                incomplete_id,
                                serenity::builder::EditMessage::new().content(text),
                            )
                            .await
                            .context("updating message on Discord")?;
                        *incomplete = Some((edited.id, text.to_string()));
                    }
                }
            } else if !text.is_empty() {
                let msg = self
                    .channel_id
                    .send_message(&self.http, CreateMessage::new().content(text))
                    .await
                    .context("sending update to Discord")?;
                *incomplete = Some((msg.id, text.to_string()));
            }
        }

        drop(incomplete);

        Ok(())
    }
}

#[async_trait]
impl PlatformInstance for DiscordPlatformInstance {
    fn message_receiver(&self) -> broadcast::Receiver<messages::PlatformMessage> {
        self.message_rx.resubscribe()
    }

    async fn set_working(&self, is_working: bool) -> Result<()> {
        *self.working.lock().await = is_working;
        if is_working && let Err(e) = self.http.broadcast_typing(self.channel_id).await {
            warn!("Error broadcasting typing at start of work: {e:?}");
        }
        Ok(())
    }

    async fn get_username(&self) -> Result<String> {
        let user = self
            .http
            .get_current_user()
            .await
            .context("getting current user")?;
        Ok(user.name.clone())
    }

    async fn set_username(&self, username: &str) -> Result<()> {
        debug!("Discord set_username: {username}");
        Ok(())
    }

    async fn send_partial<'a, 'b>(&self, partial_update: PartialUpdate<'a, 'b>) -> Result<()> {
        let m_text = match partial_update {
            PartialUpdate::Real(entry, usage) => match entry {
                messages::BotEntryKind::BotMessage(bot_message) => {
                    let mut x = String::new();

                    for content in &bot_message.content {
                        let messages::BotMessageContent::Text(bot_text_content) = content else {
                            return Ok(());
                        };

                        if !x.is_empty() {
                            x.push('\n');
                        }

                        x.push_str(&bot_text_content.text);
                    }

                    let underscore_escaped = x.replace('_', "\\_");

                    Some(format!("✍️ _{underscore_escaped}_"))
                }
                messages::BotEntryKind::ToolCall(tool_call) => {
                    let tool_status_message = utils::standard_tool_status_message(tool_call);
                    Some(utils::standard_status_str(&tool_status_message))
                }
                messages::BotEntryKind::RemoteToolCall(_) => {
                    Some("doin a remote tool call".to_string())
                }
                messages::BotEntryKind::Reasoning(_reasoning) => {
                    let config = snapshot_config(&self.config).await;

                    Some(usage.map_or_else(
                        || utils::standard_status_str("workin on a big thought"),
                        |usage| {
                            utils::standard_status_str(&format!(
                                "workin on a big thought: {}%",
                                (usage.tokens_generated * 100)
                                    / config.providers.llm.default_constraints.max_think
                            ))
                        },
                    ))
                }
            },
            PartialUpdate::Dream(forced, dream_state, pct) => {
                self.dream_status_update(dream_state, pct).await?;
                if forced {
                    Some(utils::standard_status_str(&format!(
                        "out of context - forced dream state: {dream_state} {pct}%",
                    )))
                } else {
                    None
                }
            }
        };

        if let Some(text) = m_text {
            self.send_text_update(&text).await?;
        }

        Ok(())
    }

    async fn send_final(
        &self,
        message: Result<(&messages::BotMessage, Option<&llm::openai::Usage>, bool), &str>,
    ) -> Result<Vec<messages::PlatformMessage>> {
        debug!("Sending final message to Discord");

        let bot_id = self.bot_id.read().await;
        let bot_id_str = bot_id.as_ref().context("retrieving bot ID")?.clone();
        drop(bot_id);

        let mut incomplete = self.incomplete_message.lock().await;

        let mut sent_messages = Vec::new();

        if let Some((incomplete_id, _incomplete_text)) = &*incomplete {
            self.channel_id
                .delete_message(&self.http, incomplete_id)
                .await
                .context("deleting message on Discord")?;
        }

        // Clear the incomplete message tracking
        *incomplete = None;
        drop(incomplete);

        let mut attachments: Vec<CreateAttachment> = Vec::new();
        let mut full_text = String::new();

        match message {
            Ok(m) => {
                for content in &m.0.content {
                    match content {
                        messages::BotMessageContent::Text(t) => full_text.push_str(&t.text),
                        messages::BotMessageContent::Attachment(attachment) => {
                            attachments.push(
                                CreateAttachment::path(&attachment)
                                    .await
                                    .context("Loading attachment")?,
                            );
                        }
                    }
                }
            }
            Err(err) => {
                full_text.push_str(&utils::standard_status_str(err));
            }
        }

        if attachments.is_empty() && full_text.is_empty() {
            return Ok(Vec::new());
        }

        let split_text = if full_text.trim().is_empty() {
            vec![String::new()]
        } else {
            split_string_into_chunks(&full_text, 2000, 1500)
        };
        let split_text_len = split_text.len();

        for (i, text) in split_text.into_iter().enumerate() {
            debug!("Sending chunk {}/{}", i + 1, split_text_len);

            let mut m = CreateMessage::new().content(text);

            if i == split_text_len - 1 {
                m = m.add_files(attachments.clone());
            }

            let msg = self
                .channel_id
                .send_message(&self.http, m)
                .await
                .context("sending final to Discord")?;

            sent_messages.push(msg);
        }

        let username = self.get_username().await.context("getting username")?;

        Ok(sent_messages
            .into_iter()
            .map(|s| messages::PlatformMessage {
                id: s.id.to_string(),
                author_id: messages::PlatformUserId(bot_id_str.clone()),
                author_name: username.clone(),
                text: s.content,
                mentions: vec![],
                attachments: vec![],
            })
            .collect())
    }

    async fn set_status(&self, status: &str) -> Result<()> {
        let mut l = self.last_status.lock().await;
        if let Some(last_status) = &*l
            && last_status == status
        {
            return Ok(());
        }

        *l = Some(status.to_string());
        drop(l);

        self.context
            .set_presence(Some(ActivityData::custom(status)), OnlineStatus::Online);

        Ok(())
    }

    async fn set_avatar(&self, avatar: &Path) -> Result<()> {
        let attachment = CreateAttachment::path(&avatar).await?;

        let mut current_user = self
            .http
            .get_current_user()
            .await
            .context("getting current user")?;

        current_user
            .edit(&self.http, EditProfile::new().avatar(&attachment))
            .await
            .context("editing profile avatar")?;

        info!("Updated bot avatar");
        Ok(())
    }

    async fn chat_info(&self) -> Result<String> {
        let channel = self.http.get_channel(self.channel_id).await?;
        let x = match channel {
            serenity::all::Channel::Guild(guild_channel) => {
                let guild = self.http.get_guild(guild_channel.guild_id).await?;

                format!(
                    "Discord channel {} in guild {}",
                    guild_channel.name, guild.name
                )
            }
            serenity::all::Channel::Private(private_channel) => {
                format!("Private discord DM with {}", private_channel.recipient.name)
            }
            _ => "Unknown channel type".to_string(),
        };

        Ok(x)
    }

    fn instance_id(&self) -> String {
        format!("discord_{}", self.channel_id)
    }
}

fn split_string_into_chunks(
    text: &str,
    max_chunk_size: usize,
    min_preferred_size: usize,
) -> Vec<String> {
    if text.len() <= max_chunk_size {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let mut last_newline = None;
    let mut last_space = None;

    for (i, c) in text.char_indices() {
        // Track potential break points
        if c == '\n' {
            last_newline = Some(i);
            last_space = None; // Newline takes priority over space
        } else if c == ' ' && last_newline.is_none() {
            last_space = Some(i);
        }

        // Check if we need to split
        if i - start >= max_chunk_size {
            // Find the best break point
            let split_at =
                last_newline.unwrap_or_else(|| last_space.unwrap_or(start + max_chunk_size));

            // Only use the break point if it gives us a reasonable chunk size
            let actual_split =
                if split_at - start >= min_preferred_size || split_at >= i - max_chunk_size {
                    split_at
                } else {
                    start + max_chunk_size
                };

            // Create the chunk (up to but not including the break character)
            let chunk = &text[start..actual_split];
            chunks.push(chunk.to_string());

            // Skip the break character if it was a newline or space
            start = if last_newline.is_some() || last_space.is_some() {
                actual_split + 1
            } else {
                actual_split
            };

            last_newline = None;
            last_space = None;
        }
    }

    // Add the remaining chunk
    if start < text.len() {
        chunks.push(text[start..].to_string());
    }

    chunks
}
