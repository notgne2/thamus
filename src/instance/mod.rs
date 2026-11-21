use anyhow::{Context as _, Result, anyhow};
use formatx::formatx;
use futures::stream::StreamExt;
use indoc::indoc;
use jiff::{Span, Timestamp};
use serde::{Deserialize, Serialize};
use serde_inline_default::serde_inline_default;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use tokio::sync::{Semaphore, TryAcquireError};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::CommonResources;
use crate::agent;
use crate::config::{ConfigSnapshot, SharedConfig, snapshot_config};
use crate::db;
use crate::instance::smarts::dream;
use crate::messages;
use crate::platforms::{PartialUpdate, PlatformInstance};
use crate::providers::llm;
use crate::utils::{NotCow, spawn_named, tooling};

mod instructions;
mod smarts;
mod tools_def;

pub use smarts::dream::DreamState;

/// Configuration for the chat characteristics of the instance
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatConfig {
    /// After not being spoken to for this long, require a ping
    pub timeout_after: Span,
    /// Regexes that make a message wake up the instance
    pub alias_regexes: Vec<String>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            timeout_after: <Span as std::str::FromStr>::from_str("3m").unwrap(),
            alias_regexes: vec![],
        }
    }
}

/// Definition of a voice to be used by the instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceDefinition {
    /// Name of the voice
    pub name: String,
    /// Path to the reference audio for the voice
    pub ref_file: String,
    /// Transcription of the reference audio
    pub ref_text: String,
    /// A card describing the character of the voice
    #[serde(default)]
    pub char_card: Option<String>,
}

/// Configuration for the voice characteristics of the instance
#[serde_inline_default]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// List of voices available for use
    pub voices: Vec<VoiceDefinition>,

    /// Default voice to use for talking
    pub default_voice: Option<String>,

    /// How long before a user is considered to be no-longer speaking
    pub voice_silence_ms: u64,

    /// Path to an audio file to play while thinking
    pub thinking_wav: Option<PathBuf>,

    /// Path to an audio file to play while working
    pub working_wav: Option<PathBuf>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            voices: Vec::new(),
            default_voice: None,
            voice_silence_ms: 900,
            thinking_wav: None,
            working_wav: None,
        }
    }
}

/// Configuration for message templates used by the instance
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagesConfig {
    pub public_error: String,
    pub private_error: String,
}

impl Default for MessagesConfig {
    fn default() -> Self {
        Self {
            public_error: String::from("Error generating response: {error}"),
            #[allow(clippy::literal_string_with_formatting_args)]
            private_error: String::from(indoc! {"
                Error in previous turn writing response:
                {error:#?}
            "}),
        }
    }
}

/// Configuration for memory tools
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub search_limit: u16,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { search_limit: 8 }
    }
}

/// Configuration for the instance
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceConfig {
    /// Configure smarts
    pub smarts: smarts::SmartsConfig,
    /// Configure the instructions
    pub prompts: instructions::InstructionsConfig,
    /// Configure voice
    pub voice: VoiceConfig,
    /// How this instance behaves in chat
    pub chat: ChatConfig,
    /// Configures how memory is used
    pub memory: MemoryConfig,
    /// Message templates
    pub messages: MessagesConfig,
    /// Tooling settings
    pub tooling: tooling::ToolingConfig,
}

#[derive(Debug, Clone)]
pub struct InstanceProps {
    pub tools: bool,
    pub thinking: bool,
    pub voice: bool,
}

pub struct Instance {
    id: String,
    #[allow(clippy::struct_field_names)]
    platform_instance: Arc<dyn PlatformInstance>,
    common: CommonResources,
    dream_lock: Arc<Semaphore>,
    work_lock_budget: Arc<AtomicU8>,
    props: InstanceProps,

    agent: Arc<agent::Agent>,
}

#[derive(Clone)]
pub struct InstanceAgentConfigSource {
    shared_config: SharedConfig,
}

#[async_trait::async_trait]
impl agent::AgentConfigSource for InstanceAgentConfigSource {
    async fn get_config(&'_ self) -> NotCow<'_, agent::AgentConfig> {
        let config_snapshot = snapshot_config(&self.shared_config).await;
        let default_agent = config_snapshot.default_agent();

        NotCow::Owned(agent::AgentConfig {
            messages: agent::AgentMessagesConfig {
                error: config_snapshot.instance.messages.private_error.clone(),
                ..default_agent.messages.clone()
            },
            tooling: config_snapshot.instance.tooling.clone(),
            ..default_agent.clone()
        })
    }
}

#[async_trait::async_trait]
impl agent::AgentConfigSource for &Instance {
    async fn get_config(&'_ self) -> NotCow<'_, agent::AgentConfig> {
        let config_snapshot = snapshot_config(&self.common.config).await;
        let default_agent = config_snapshot.default_agent();

        NotCow::Owned(agent::AgentConfig {
            messages: agent::AgentMessagesConfig {
                error: config_snapshot.instance.messages.private_error.clone(),
                ..default_agent.messages.clone()
            },
            tooling: config_snapshot.instance.tooling.clone(),
            ..default_agent.clone()
        })
    }
}

#[async_trait::async_trait]
impl agent::AgentToolsetSource for &Instance {
    async fn get_toolset(&'_ self) -> Result<NotCow<'_, tooling::BotToolSet>> {
        let config_snapshot = snapshot_config(&self.common.config).await;

        let toolset = Instance::get_toolset(&self, &config_snapshot)
            .await
            .context("Getting toolset")?;

        Ok(NotCow::Owned(toolset))
    }
}

#[async_trait::async_trait]
impl agent::AgentInstructionsSource for &Instance {
    async fn get_instructions(&self) -> Result<Cow<str>> {
        let config_snapshot = snapshot_config(&self.common.config).await;

        let chat_info = self
            .platform_instance
            .chat_info()
            .await
            .context("Getting chat info from platform instance")?;

        let instructions = instructions::make_instructions(
            &config_snapshot.instance.prompts,
            &config_snapshot.instance.voice,
            &self.props,
            Some(chat_info.as_str()),
        )
        .context("Making instructions")?;

        Ok(Cow::Owned(instructions))
    }
}

impl Instance {
    #[instrument(name = "instance", level = "debug", skip_all)]
    pub async fn run(
        platform_instance: Arc<dyn PlatformInstance>,
        common: CommonResources,
        props: InstanceProps,
    ) -> Result<!> {
        let work_lock_budget = Arc::new(AtomicU8::new(0));

        let agent = agent::Agent::new(
            common.clone(),
            work_lock_budget.clone(),
            platform_instance.instance_id(),
        );

        let s = Arc::new(Self {
            id: platform_instance.instance_id(),
            platform_instance,
            common,
            props,
            dream_lock: Arc::new(Semaphore::new(1)),
            work_lock_budget,
            agent,
        });

        s.set_status_from_saved()
            .await
            .context("Failed to set current status")?;

        s.clone().start_receiving().await?;
    }

    #[instrument(level = "trace", skip(self))]
    async fn get_toolset(&self, config_snapshot: &ConfigSnapshot) -> Result<tooling::BotToolSet> {
        let common = &self.common;

        if !self.props.tools {
            return Ok(tooling::BotToolSet::create(false, vec![]));
        }

        let instance_agent_config_source = InstanceAgentConfigSource {
            shared_config: Arc::clone(&self.common.config),
        };

        let mut tools: Vec<Box<dyn tooling::BotTool>> = vec![
            Box::new(tools_def::WorkLock(Arc::clone(&self.work_lock_budget))),
            Box::new(tools_def::SetAvatar(
                self.platform_instance.clone(),
                instance_agent_config_source.clone(),
            )),
            Box::new(tools_def::SetStatus(
                self.platform_instance.clone(),
                self.common.pool.clone(),
                self.id.clone(),
            )),
            Box::new(tools_def::CheckTime),
            Box::new(tools_def::WebSearch),
            Box::new(tools_def::run::RunCommand(
                instance_agent_config_source.clone(),
            )),
            Box::new(tools_def::file::ReadTextFile(
                instance_agent_config_source.clone(),
            )),
            Box::new(tools_def::file::EditFile(
                instance_agent_config_source.clone(),
            )),
            Box::new(tools_def::file::WriteFile(
                instance_agent_config_source.clone(),
            )),
            Box::new(tools_def::file::ViewMedia(
                instance_agent_config_source.clone(),
            )),
        ];

        if config_snapshot.providers.audio_cpp_uri.is_some() {
            tools.push(Box::new(tools_def::speech::GenerateSpeech(common.clone())));
            tools.push(Box::new(tools_def::song::GenerateSong(common.clone())));
        }

        if config_snapshot.providers.sd_cpp_uri.is_some() {
            tools.push(Box::new(tools_def::image::GenerateImage(
                common.clone(),
                instance_agent_config_source.clone(),
            )));
            tools.push(Box::new(tools_def::image::EditImage(
                common.clone(),
                instance_agent_config_source.clone(),
            )));
        }

        if config_snapshot.providers.embed.is_some() {
            tools.push(Box::new(tools_def::memory::SearchMemory(common.clone())));
            tools.push(Box::new(tools_def::memory::SaveMemory(common.clone())));
            tools.push(Box::new(tools_def::memory::DeleteMemory(common.clone())));
        }

        tools.extend(
            self.platform_instance
                .platform_tools()
                .await
                .context("Getting platform tools")?,
        );

        Ok(tooling::BotToolSet::create(true, tools))
    }

    #[instrument(level = "trace", skip_all)]
    async fn should_respond(
        &self,
        config_snapshot: &ConfigSnapshot,
        platform_msg: &messages::PlatformMessage,
    ) -> anyhow::Result<bool> {
        if platform_msg.text == "norespond" {
            return Ok(false);
        }

        let username = self
            .platform_instance
            .get_username()
            .await
            .context("Getting username")?;
        for mention in &platform_msg.mentions {
            if mention.name == username {
                return Ok(true);
            }
        }

        for r in &config_snapshot.instance.chat.alias_regexes {
            if regex::Regex::new(r)
                .context("Compiling chat alias regex")?
                .is_match(&platform_msg.text)
            {
                return Ok(true);
            }
        }

        let (last_assistant_msg_time, stopped_since_talked) =
            db::get_last_response_info(&self.common.pool, &self.id)
                .await
                .context("checking recent history response state")?;

        if stopped_since_talked {
            return Ok(false);
        }

        if (Timestamp::now() - last_assistant_msg_time)
            .compare(config_snapshot.instance.chat.timeout_after)
            .context("Comparing timeout")?
            == Ordering::Less
        {
            return Ok(true);
        }

        debug!("No reason to respond to message");

        Ok(false)
    }

    async fn make_user_message(
        &self,
        config_snapshot: &ConfigSnapshot,
        platform_msg: &messages::PlatformMessage,
    ) -> Result<messages::UserMessage> {
        let mut content = vec![];

        for received_attachment in &platform_msg.attachments {
            let attachment_path = received_attachment
                .save(&self.common.http, &config_snapshot.storage.attachments_dir)
                .await
                .context("Saving received attachment")?;

            content.push(messages::MessageContent::Attachment(attachment_path));
        }

        content.push(messages::MessageContent::Text(platform_msg.text.clone()));

        Ok(messages::UserMessage {
            content,
            username: platform_msg.author_name.clone(),
        })
    }

    #[instrument(level = "info", skip_all)]
    async fn handle_response_error(
        &self,
        config_snapshot: &ConfigSnapshot,
        error: &anyhow::Error,
    ) -> Result<String> {
        let public_error_text = formatx!(
            &config_snapshot.instance.messages.public_error,
            error = error
        )
        .context("formatting public response error template")?;

        db::history::insert(
            &self.common.pool,
            &self.id,
            &messages::MessageEntry::Error(public_error_text.clone()),
        )
        .await
        .context("inserting public response error entry into history")?;

        Ok(public_error_text)
    }

    #[instrument(skip_all)]
    async fn respond(
        self: &Arc<Self>,
        config_snapshot: &ConfigSnapshot,
        msg: messages::MessageEntry,
    ) -> Result<Option<u32>> {
        let Some(s) =
            Box::pin(
                self.agent
                    .respond(self.as_ref(), config_snapshot, None, msg.clone(), false),
            )
            .await
            .context("Starting response stream")?
        else {
            return Ok(None);
        };

        futures::pin_mut!(s);

        let mut last_usage = None;

        while let Some(r) = s.next().await {
            let x = match r {
                Ok(x) => x,
                Err(err) => {
                    let config_snapshot = snapshot_config(&self.common.config).await;

                    if let agent::ResponseError::LlmCall(llm::CallError::OpenAI(
                        llm::openai::CallError::Server(_status, ref body),
                    )) = err
                        && body.get("type")
                            == Some(&serde_json::Value::String(
                                "exceed_context_size_error".to_string(),
                            ))
                    {
                        warn!("Context size exceeded, forcing dream");

                        if let Err(dream_err) =
                            Box::pin(self.trigger_dream(&config_snapshot, None)).await
                        {
                            let text = self
                                .handle_response_error(
                                    &config_snapshot,
                                    &anyhow!("Dreaming away overflowed context: {dream_err}"),
                                )
                                .await?;
                            self.platform_instance
                                .send_final(Err(&text))
                                .await
                                .context("sending final bot dream error message")?;
                        }

                        return Box::pin(Arc::clone(self).respond(&config_snapshot, msg)).await;
                    }

                    let text = self
                        .handle_response_error(&config_snapshot, &anyhow!("Responding: {err}"))
                        .await?;
                    self.platform_instance
                        .send_final(Err(&text))
                        .await
                        .context("sending final bot error message")?;

                    return Err(err).context("Response stream item");
                }
            };

            if let agent::AgentOutput::FinalMessage {
                usage: Some(usage), ..
            } = &x
                && last_usage.is_none()
            {
                last_usage =
                    Some(usage.input_tokens + usage.cached_tokens + usage.tokens_generated);
            }

            match x {
                agent::AgentOutput::FinalMessage {
                    message,
                    usage,
                    complete,
                } => {
                    self.platform_instance
                        .send_final(Ok((&message, usage.as_ref(), complete)))
                        .await
                        .context("sending final bot message")?;
                }
                agent::AgentOutput::PartialEntry { entry_kind, usage } => {
                    let platform_instance = Arc::clone(&self.platform_instance);
                    spawn_named("send_partial", async move {
                        if let Err(e) = platform_instance
                            .send_partial(PartialUpdate::Real(&entry_kind, usage.as_ref()))
                            .await
                        {
                            error!("failed to send real partial update: {e}");
                        }
                    });
                }
                agent::AgentOutput::End { usage } => {
                    last_usage =
                        Some(usage.input_tokens + usage.cached_tokens + usage.tokens_generated);
                }
            }
        }

        Ok(last_usage)
    }

    #[instrument(skip_all)]
    async fn receive(self: &Arc<Self>, platform_msg: &messages::PlatformMessage) -> Result<()> {
        if platform_msg.text.to_lowercase().starts_with("secret ") {
            info!("Skipping handling of secret message");
            return Ok(());
        }

        let config_snapshot = snapshot_config(&self.common.config).await;

        let msg = messages::MessageEntry::UserMessage(
            self.make_user_message(&config_snapshot, platform_msg)
                .await
                .context("Making user message")?,
        );

        if !self
            .should_respond(&config_snapshot, platform_msg)
            .await
            .context("Checking should respond")?
        {
            self.agent.insert(msg).await?;
            return Ok(());
        }

        if let Err(err) = self.platform_instance.set_working(true).await {
            error!("Failed to set working state on platform: {err:#?}");
        }

        if let Some(infusion_msg) = smarts::infusion::create_infusion_message(
            &self.common,
            &config_snapshot.instance.smarts.infusion,
            config_snapshot.providers.embed.as_ref(),
            &self.id,
            platform_msg,
        )
        .await
        .context("Creating infusion message")?
        {
            self.agent
                .insert(infusion_msg)
                .await
                .context("Inserting infusion message")?;
        }

        let respond_r = Box::pin(self.respond(&config_snapshot, msg)).await;

        if let Err(err) = self.platform_instance.set_working(false).await {
            error!("Failed to set working state on platform: {err:#?}");
        }

        let last_total_tokens = respond_r.context("Responding to message")?;

        if let Some(total_tokens) = last_total_tokens {
            trace!("Got total tokens after response: {total_tokens}");

            let self_ = Arc::clone(self);
            spawn_named("maybe_trigger_dream", async move {
                if let Err(e) = self_
                    .trigger_dream(&config_snapshot, Some(total_tokens))
                    .await
                {
                    error!("Failed to passively dream: {e:#?}");
                }
            });
        } else {
            warn!("Somehow never got total tokens");
        }

        Ok(())
    }

    async fn start_receiving(self: Arc<Self>) -> Result<!> {
        let mut receiver = self.platform_instance.message_receiver();

        loop {
            let platform_msg = receiver
                .recv()
                .await
                .context("Using platform instance message receiver")?;

            let self_ = Arc::clone(&self);
            spawn_named("instance_receive", async move {
                if let Err(err) = Box::pin(self_.receive(&platform_msg)).await {
                    error!("Error handling platform message: {:?}", err);
                }
            });
        }
    }

    #[instrument(level = "trace", skip(self))]
    async fn set_status_from_saved(&self) -> Result<()> {
        if let Some(status) = db::get_status(&self.common.pool, &self.id)
            .await
            .context("Failed to get saved status")?
        {
            self.platform_instance
                .set_status(&status)
                .await
                .context("setting platform instance's status post-dream status")?;
        }

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    async fn trigger_dream(
        &self,
        config_snapshot: &ConfigSnapshot,
        context_size: Option<u32>,
    ) -> Result<()> {
        let insert_compaction_after = Box::pin(dream::prep_if_should_dream(
            &self.common,
            config_snapshot,
            &self.id,
            context_size,
        ))
        .await
        .context("Preparing dream if should dream")?;

        if let Some(insert_compaction_after) = insert_compaction_after {
            let dream_lock = match self.dream_lock.try_acquire() {
                Ok(l) => l,
                Err(TryAcquireError::NoPermits) => {
                    if context_size.is_none() {
                        trace!(
                            "Dream is forced and another is already ongoing - waiting for it, then skipping"
                        );
                        let _ = self.dream_lock.acquire().await?;
                    } else {
                        trace!("Dream is passive, and another is already ongoing - skipping");
                    }
                    return Ok(());
                }
                Err(e) => return Err(e).context("Trying to acquire dream lock"),
            };

            let r = Box::pin(smarts::dream::run_dream(
                self.common.clone(),
                config_snapshot,
                &self.id,
                insert_compaction_after,
                |dream_state, pct| {
                    let platform_instance = Arc::clone(&self.platform_instance);
                    spawn_named("send_dream_partial", async move {
                        if let Err(e) = platform_instance
                            .send_partial(PartialUpdate::Dream(
                                context_size.is_none(),
                                dream_state,
                                pct,
                            ))
                            .await
                        {
                            error!("failed to send forced dream status: {}", e);
                        }
                    });
                },
            ))
            .await;

            // in case the instance updated its status during the dream? todo not always
            self.set_status_from_saved()
                .await
                .context("Failed to set current status")?;

            drop(dream_lock);

            r.context("Dreaming")?;
        }

        Ok(())
    }
}
