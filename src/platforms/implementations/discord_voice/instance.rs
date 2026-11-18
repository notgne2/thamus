use crate::providers::generate::decode_speech;
use dashmap::DashMap;
use serenity::all::{GuildId, UserId};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use nanoid::nanoid;
use serenity::all::{
    ActivityData, ChannelId, Context, CreateAttachment, EditProfile, OnlineStatus,
};
use serenity::http::Http;
use songbird::tracks::TrackHandle;
use std::fmt::Write as _;
use tokio::fs::OpenOptions;
use tokio::io::AsyncSeekExt;
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, error, info, trace, warn};

use anyhow::{Context as _, Result, bail};

use crate::config::{ConfigSnapshot, SharedConfig, snapshot_config};
use crate::messages::{self};
use crate::platforms::{PartialUpdate, PlatformInstance};
use crate::providers::generate::generate_speech;
use crate::providers::llm;

/// Discord platform implementation for a specific channel
pub struct DiscordVoicePlatformInstance {
    pub config: SharedConfig,

    pub http: reqwest::Client,

    pub channel_id: ChannelId,
    pub guild_id: serenity::all::GuildId,
    pub serenity_http: std::sync::Arc<Http>,
    pub message_tx: broadcast::Sender<messages::PlatformMessage>,
    pub message_rx: broadcast::Receiver<messages::PlatformMessage>,
    pub bot_id: std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
    pub context: Context,
    pub outputted_pos: Mutex<usize>,
    pub working_track: Mutex<Option<(TrackHandle, bool)>>,
}

struct TrackErrorNotifier;

#[async_trait]
impl songbird::EventHandler for TrackErrorNotifier {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        if let songbird::EventContext::Track(track_list) = ctx {
            for (state, handle) in *track_list {
                error!(
                    track_id = %handle.uuid(),
                    playing = ?state.playing,
                    "Songbird track state/error event"
                );
            }
        }

        None
    }
}

async fn log_audio_file(path: &Path) -> Result<()> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("reading metadata for audio file {}", path.display()))?;
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading audio file header {}", path.display()))?;
    let header_len = bytes.len().min(12);

    trace!(
        path = %path.display(),
        size = metadata.len(),
        header = ?&bytes[..header_len],
        "Audio file queued for Songbird"
    );

    Ok(())
}

impl DiscordVoicePlatformInstance {
    pub fn receiver(&self) -> Receiver {
        Receiver::new(
            self.message_tx.clone(),
            self.config.clone(),
            self.http.clone(),
            self.guild_id,
            self.context.clone(),
        )
    }

    async fn speak_words(
        &self,
        config_snapshot: &ConfigSnapshot,
        _discord_voice_config: &super::DiscordVoiceConfig,
        client: &reqwest::Client,
        audio_cpp_uri: &str,
        words: &str,
    ) -> Result<()> {
        debug!("Got voice response chunk: {words}");

        let mut working_track_handle = self.working_track.lock().await;
        if let Some((handle, _)) = working_track_handle.as_ref() {
            debug!("Stopping working track");

            if let Err(e) = handle.stop() {
                warn!("Failed to stop working track to speak words: {}", e);
            }
        }
        *working_track_handle = None;
        drop(working_track_handle);

        let id = nanoid!();
        let save_path = config_snapshot.storage.voice_dir.join(format!("{id}.wav"));

        let mut voice = None;

        for v in &config_snapshot.instance.voice.voices {
            if Some(v.name.as_str()) == config_snapshot.instance.voice.default_voice.as_deref() {
                voice = Some(v);
                break;
            }
        }

        let Some(voice) = voice else {
            return Err(anyhow::anyhow!("Can't find default voice"));
        };

        {
            let mut voice_file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&save_path)
                .await?;

            generate_speech(
                client,
                audio_cpp_uri,
                "omnivoice",
                voice.ref_file.as_str(),
                voice.ref_text.as_str(),
                words,
                &mut voice_file,
            )
            .await?;
        }

        let manager = songbird::get(&self.context)
            .await
            .context("Songbird Voice client placed in at initialisation")?
            .clone();

        log_audio_file(&save_path).await?;
        trace!("Playing voice file: {}", save_path.display());

        let track = {
            let handler_mutex = manager.get(self.guild_id).context("not in voice channel")?;
            let mut handler = handler_mutex.lock().await;
            handler.add_global_event(
                songbird::events::TrackEvent::Error.into(),
                TrackErrorNotifier,
            );

            let f = songbird::input::File::new(save_path.clone());
            handler.enqueue_input(f.into()).await
        };

        trace!(track_id = %track.uuid(), "Songbird accepted generated voice track");

        track.set_volume(1.0)?;

        Ok(())
    }

    async fn play_working(&self, thinking: bool) -> Result<()> {
        let mut working_track_handle = self.working_track.lock().await;
        if let Some((handle, is_thinking)) = working_track_handle.as_ref() {
            if *is_thinking == thinking {
                return Ok(());
            }
            if let Err(e) = handle.stop() {
                warn!("Failed to stop previous working track: {}", e);
            }
        }

        let config_snapshot = crate::config::snapshot_config(&self.config).await;
        let p = if thinking {
            config_snapshot.instance.voice.thinking_wav.clone()
        } else {
            config_snapshot.instance.voice.working_wav.clone()
        };
        let Some(p) = p else {
            return Ok(());
        };
        trace!("Playing working track: {:?}", p);
        log_audio_file(&p).await?;

        let manager = songbird::get(&self.context)
            .await
            .context("Songbird Voice client placed in at initialisation")?
            .clone();

        let track = {
            let handler_mutex = manager.get(self.guild_id).context("not in voice channel")?;
            let mut handler = handler_mutex.lock().await;
            handler.add_global_event(
                songbird::events::TrackEvent::Error.into(),
                TrackErrorNotifier,
            );

            let f = songbird::input::File::new(p);
            handler.play_input(f.into())
        };

        trace!(track_id = %track.uuid(), "Songbird accepted working track");

        track.set_volume(0.3)?;
        *working_track_handle = Some((track, thinking));
        drop(working_track_handle);

        Ok(())
    }

    async fn speak_update_text(
        &self,
        config_snapshot: &ConfigSnapshot,
        discord_voice_config: &super::DiscordVoiceConfig,
        client: &reqwest::Client,
        audio_cpp_uri: &str,
        from_update: bool,
        text: &str,
    ) -> Result<()> {
        let mut outputted_pos = self.outputted_pos.lock().await;

        let mut start = *outputted_pos;
        if start > text.len() || !text.is_char_boundary(start) {
            start = 0;
        }

        if !from_update {
            let sentence = &text[start..];
            if !sentence.is_empty() {
                trace!("final remaining sentence is {sentence}");
                self.speak_words(
                    config_snapshot,
                    discord_voice_config,
                    client,
                    audio_cpp_uri,
                    sentence,
                )
                .await?;
            }
            *outputted_pos = text.len();
            return Ok(());
        }

        let is_separator = |c: char| matches!(c, '.' | '?' | '!' | '！' | '？' | '。' | '…');

        let chars: Vec<(usize, char)> = text.char_indices().collect();

        let mut char_idx = chars
            .iter()
            .position(|&(idx, _)| idx >= start)
            .unwrap_or(chars.len());

        while char_idx < chars.len() {
            if let Some(rel_punc_idx) = chars[char_idx..].iter().position(|&(_, c)| is_separator(c))
            {
                let punc_start = char_idx + rel_punc_idx;

                let mut punc_end = punc_start + 1;
                while punc_end < chars.len() && is_separator(chars[punc_end].1) {
                    punc_end += 1;
                }

                let mut next_char_idx = punc_end;
                while next_char_idx < chars.len() && chars[next_char_idx].1.is_whitespace() {
                    next_char_idx += 1;
                }

                if next_char_idx < chars.len() && chars[next_char_idx].1.is_alphanumeric() {
                    let actual_end = chars[next_char_idx].0;
                    let sentence = &text[start..actual_end];

                    trace!("final sentence is {sentence}");

                    self.speak_words(
                        config_snapshot,
                        discord_voice_config,
                        client,
                        audio_cpp_uri,
                        sentence,
                    )
                    .await?;

                    start = actual_end;
                    char_idx = next_char_idx;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        *outputted_pos = start;
        drop(outputted_pos);

        Ok(())
    }
}

#[async_trait]
impl PlatformInstance for DiscordVoicePlatformInstance {
    fn message_receiver(&self) -> broadcast::Receiver<messages::PlatformMessage> {
        self.message_rx.resubscribe()
    }

    async fn set_working(&self, is_working: bool) -> Result<()> {
        if is_working {
            self.play_working(false).await?;
        }
        Ok(())
    }

    async fn get_username(&self) -> Result<String> {
        let user = self
            .serenity_http
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
        let config_snapshot = snapshot_config(&self.config).await;

        let Some(discord_voice_config) = &config_snapshot.platforms.discord_voice else {
            bail!("Discord voice disabled");
        };

        let Some(audio_cpp_uri) = &config_snapshot.providers.audio_cpp_uri else {
            bail!("Audio generation disabled");
        };

        let text = match partial_update {
            PartialUpdate::Real(entry, _token_info) => match entry {
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

                    x
                }
                messages::BotEntryKind::ToolCall(_)
                | messages::BotEntryKind::RemoteToolCall(_)
                | messages::BotEntryKind::Reasoning(_) => {
                    self.play_working(true).await?;
                    return Ok(());
                }
            },
            PartialUpdate::Dream(forced, _dream_state, _pct) => {
                if forced {
                    self.play_working(true).await?;
                }
                return Ok(());
            }
        };

        self.speak_update_text(
            &config_snapshot,
            discord_voice_config,
            &self.http,
            audio_cpp_uri,
            true,
            &text,
        )
        .await?;

        Ok(())
    }

    async fn send_final(
        &self,
        message: Result<(&messages::BotMessage, Option<&llm::openai::Usage>, bool), &str>,
    ) -> Result<Vec<messages::PlatformMessage>> {
        let config_snapshot = snapshot_config(&self.config).await;

        let Some(discord_voice_config) = &config_snapshot.platforms.discord_voice else {
            bail!("Discord voice disabled");
        };

        let Some(audio_cpp_uri) = &config_snapshot.providers.audio_cpp_uri else {
            bail!("Audio generation disabled");
        };

        let mut words = String::new();

        match message {
            Ok(m) => {
                for c in &m.0.content {
                    match c {
                        messages::BotMessageContent::Text(t) => words.push_str(&t.text),
                        messages::BotMessageContent::Attachment(_) => {}
                    }
                }
            }
            Err(e) => {
                let _ = write!(words, "Error: {e}");
            }
        }

        if words.is_empty() {
            debug!("No words to send");
            return Ok(vec![]);
        }

        trace!("Got final words: {words}");

        self.speak_update_text(
            &config_snapshot,
            discord_voice_config,
            &self.http,
            audio_cpp_uri,
            false,
            &words,
        )
        .await?;
        *self.outputted_pos.lock().await = 0;

        Ok(vec![])
    }

    async fn set_status(&self, status: &str) -> Result<()> {
        self.context
            .set_presence(Some(ActivityData::custom(status)), OnlineStatus::Online);
        Ok(())
    }

    async fn set_avatar(&self, avatar: &Path) -> Result<()> {
        let attachment = CreateAttachment::path(&avatar).await?;

        let mut current_user = self
            .serenity_http
            .get_current_user()
            .await
            .context("getting current user")?;

        current_user
            .edit(&self.serenity_http, EditProfile::new().avatar(&attachment))
            .await
            .context("editing profile avatar")?;

        info!("Updated bot avatar");
        Ok(())
    }

    async fn chat_info(&self) -> Result<String> {
        let channel = self.serenity_http.get_channel(self.channel_id).await?;

        let x = match channel {
            serenity::all::Channel::Guild(guild_channel) => {
                let guild = self.serenity_http.get_guild(guild_channel.guild_id).await?;

                format!(
                    "Discord VOICE channel {} in guild {}",
                    guild_channel.name, guild.name
                )
            }
            _ => bail!("Unknown or unsupported channel type"),
        };

        Ok(x)
    }

    fn instance_id(&self) -> String {
        format!("discord_voice_{}", self.channel_id)
    }
}

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const MS_PER_TICK: u32 = 20;

struct SsrcState {
    pcm: Vec<i16>,
    silent_ticks: u32,
    has_spoken: bool,
}

fn pcm_to_wav(samples: &[i16]) -> Vec<u8> {
    let num_samples = samples.len();
    let data_bytes = num_samples * 2;
    let chunk_size = 36 + data_bytes;
    let mut out: Vec<u8> = Vec::with_capacity(44 + data_bytes);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(chunk_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * 2;
    out.extend_from_slice(&byte_rate.to_le_bytes());
    let block_align = CHANNELS * 2;
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }

    out
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

struct InnerReceiver {
    last_tick_was_empty: AtomicBool,
    ssrc_states: Mutex<HashMap<u32, SsrcState>>,
    known_voice_usernames: DashMap<u32, String>,
    message_tx: broadcast::Sender<messages::PlatformMessage>,
    config: SharedConfig,
    http: reqwest::Client,
    guild_id: GuildId,
    ctx: Context,
}

#[derive(Clone)]
pub struct Receiver {
    inner: Arc<InnerReceiver>,
}

impl Receiver {
    fn new(
        message_tx: broadcast::Sender<messages::PlatformMessage>,
        config: SharedConfig,
        http: reqwest::Client,
        guild_id: GuildId,
        ctx: Context,
    ) -> Self {
        Self {
            inner: Arc::new(InnerReceiver {
                last_tick_was_empty: AtomicBool::default(),
                known_voice_usernames: DashMap::new(),
                ssrc_states: Mutex::new(HashMap::new()),
                message_tx,
                config,
                http,
                guild_id,
                ctx,
            }),
        }
    }
}

#[async_trait]
impl songbird::EventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        let config_snapshot = snapshot_config(&self.inner.config).await;

        let Some(discord_voice_config) = &config_snapshot.platforms.discord_voice else {
            error!("Discord voice disabled");
            return None;
        };

        let Some(audio_cpp_uri) = &config_snapshot.providers.audio_cpp_uri else {
            error!("Audio generation disabled");
            return None;
        };

        match ctx {
            songbird::EventContext::SpeakingStateUpdate(songbird::model::payload::Speaking {
                speaking,
                ssrc,
                user_id,
                ..
            }) => {
                trace!(
                    "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                    user_id, ssrc, speaking,
                );
                if let Some(user_id) = user_id {
                    let name_overrides = &discord_voice_config.name_overrides;

                    let username = match self
                        .inner
                        .guild_id
                        .member(&self.inner.ctx, UserId::new(user_id.0))
                        .await
                    {
                        Ok(member) => format_name(
                            &member.user.name,
                            Some(member.display_name()),
                            name_overrides,
                        ),
                        Err(e) => {
                            error!("couldn't resolve {user_id}: {e}");
                            user_id.to_string()
                        }
                    };
                    self.inner.known_voice_usernames.insert(*ssrc, username);
                }
            }

            songbird::EventContext::VoiceTick(tick) => {
                let speaking_ssrcs: Vec<u32> = tick.speaking.keys().copied().collect();
                let is_speaking = !speaking_ssrcs.is_empty();

                self.inner
                    .last_tick_was_empty
                    .store(!is_speaking, Ordering::SeqCst);

                let mut states = self.inner.ssrc_states.lock().await;

                let ticks = (config_snapshot.instance.voice.voice_silence_ms as u32) / MS_PER_TICK;
                let threshold = ticks.max(1);

                for (ssrc, data) in &tick.speaking {
                    let Some(decoded) = data.decoded_voice.as_ref() else {
                        continue;
                    };
                    if decoded.is_empty() {
                        continue;
                    }

                    let state = states.entry(*ssrc).or_insert_with(|| SsrcState {
                        pcm: Vec::new(),
                        silent_ticks: 0,
                        has_spoken: false,
                    });

                    if !state.has_spoken {
                        state.has_spoken = true;

                        let other_ssrcs: Vec<u32> = states
                            .keys()
                            .copied()
                            .filter(|s| *s != *ssrc && !states[s].pcm.is_empty())
                            .collect();

                        for other in other_ssrcs {
                            let other_state = states.get_mut(&other).unwrap();
                            let pcm = std::mem::take(&mut other_state.pcm);
                            other_state.silent_ticks = 0;
                            let username = self
                                .inner
                                .known_voice_usernames
                                .get(&other)
                                .map(|v| v.clone());
                            let tx = self.inner.message_tx.clone();
                            tokio::spawn({
                                let audio_cpp_uri = audio_cpp_uri.clone();
                                let client = self.inner.http.clone();
                                async move {
                                    if let Err(e) =
                                        flush_pcm(pcm, username, &client, &audio_cpp_uri, &tx).await
                                    {
                                        warn!("Error flushing interrupted speaker {other}: {e:?}");
                                    }
                                }
                            });
                        }
                    }

                    let state = states.get_mut(ssrc).unwrap();
                    state.pcm.extend_from_slice(decoded);
                    state.silent_ticks = 0;
                }

                let all_ssrcs: Vec<u32> = states.keys().copied().collect();
                let mut to_flush: Vec<(u32, Vec<i16>)> = Vec::new();

                for ssrc in all_ssrcs {
                    if speaking_ssrcs.contains(&ssrc) {
                        continue;
                    }
                    let state = states.get_mut(&ssrc).unwrap();
                    if state.pcm.is_empty() {
                        continue;
                    }
                    state.silent_ticks += 1;
                    if state.silent_ticks >= threshold {
                        let pcm = std::mem::take(&mut state.pcm);
                        state.silent_ticks = 0;
                        state.has_spoken = false;
                        to_flush.push((ssrc, pcm));
                    }
                }

                for (ssrc, pcm) in to_flush {
                    let username = self
                        .inner
                        .known_voice_usernames
                        .get(&ssrc)
                        .map(|v| v.clone());
                    let tx = self.inner.message_tx.clone();
                    tokio::spawn({
                        let audio_cpp_uri = audio_cpp_uri.clone();
                        let client = self.inner.http.clone();
                        async move {
                            if let Err(e) =
                                flush_pcm(pcm, username, &client, &audio_cpp_uri, &tx).await
                            {
                                warn!("Error transcribing speech from SSRC {ssrc}: {e:?}");
                            }
                        }
                    });
                }
            }

            songbird::EventContext::RtpPacket(_) | songbird::EventContext::RtcpPacket(_) => {}

            e => {
                warn!("Received unknown voice event: {:?}", e);
            }
        }

        None
    }
}

async fn flush_pcm(
    pcm: Vec<i16>,
    username: Option<String>,
    client: &reqwest::Client,
    audio_cpp_uri: &str,
    tx: &broadcast::Sender<messages::PlatformMessage>,
) -> Result<()> {
    let username_s = username.unwrap_or_else(|| "unknown".to_string());

    if pcm.is_empty() {
        return Ok(());
    }

    let min_samples = (SAMPLE_RATE as usize * CHANNELS as usize * 500) / 1000;
    if pcm.len() < min_samples {
        return Ok(());
    }

    let wav_bytes = pcm_to_wav(&pcm);

    let text = match decode_speech(client, audio_cpp_uri, wav_bytes).await {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to transcribe voice from {username_s}: {e:?}");
            return Ok(());
        }
    };

    let text = text.trim().to_string();
    if text.is_empty() || text == "." {
        return Ok(());
    }

    info!("Transcribed voice from {username_s}: {:?}", text);

    let msg = messages::PlatformMessage {
        id: nanoid!(),
        author_id: messages::PlatformUserId(username_s.clone()),
        author_name: username_s.clone(),
        text,
        mentions: vec![],
        attachments: vec![],
    };

    if let Err(e) = tx.send(msg) {
        debug!("No receivers for voice message from {username_s}: {e}");
    }

    Ok(())
}
