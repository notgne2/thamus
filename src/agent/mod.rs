use anyhow::{Context as _, Result, anyhow};
use async_stream::{stream, try_stream};
use formatx::formatx;
use futures::Stream;
use futures::stream::StreamExt;
use indoc::indoc;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, Semaphore};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::CommonResources;
use crate::config::ConfigSnapshot;
use crate::config::SharedConfig;
use crate::config::snapshot_config;
use crate::db;
use crate::messages;
use crate::providers::llm;
use crate::utils::sandbox::GetName as _;
use crate::utils::sandbox::make_attachment_from_sandbox;
use crate::utils::{NotCow, tooling};

mod prompt;
mod slots;
mod utils;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentBehaviorConfig {
    /// Special string for the agent to output which counts as an empty response
    pub no_response_marker: String,
    /// How many turns should it get to respond (when work lock is inactive)
    pub turns: u8,
}

impl Default for AgentBehaviorConfig {
    fn default() -> Self {
        Self {
            no_response_marker: "[NULL_TRANSMISSION]".to_owned(),
            turns: 16,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentMessagesConfig {
    /// Template for response error messages
    pub error: String,
    /// Template for tool call error responses
    pub tool_call_error: String,
    /// Template for when the agent embeds an invalid path
    pub embed_path_error: String,
    /// Template for when the agent was cut-off while writing a tool call
    pub tool_call_cutoff: String,
    /// Template for when the agent was cut-off while writing a remote tool call
    pub remote_tool_call_cutoff: String,
    /// Template for when the agent was cut-off while writing reasoning
    pub reasoning_cutoff: String,
    /// Template for when the agent was cut-off while writing response
    pub response_cutoff: String,
    /// Template for telling the agent that a work lock is currently active
    pub work_lock: String,
    /// Template for telling the agent that the selfserve config is invalid
    pub selfserve_config_error: String,
}

impl Default for AgentMessagesConfig {
    fn default() -> Self {
        #[allow(clippy::literal_string_with_formatting_args)]
        Self {
            error: String::from(indoc! {"
                Error:
                {error:#?}
            "}),
            tool_call_error: String::from(indoc! {"
                Tool call error:
                {error:#?}
            "}),
            embed_path_error: String::from(indoc! {"
                Failed to read the embedded path \"{path}\" - file was not included:
                {error:#?}
            "}),
            tool_call_cutoff: String::from(indoc! {"
                Tool call was cut off and invalid, avoid going over character limit

                Tool call before cutoff (do not re-attempt):
                {tool_call}
            "}),
            remote_tool_call_cutoff: String::from(indoc! {"
                Your remote tool call was cut off and invalid, avoid going over character limit.

                Remote tool call before cutoff (do not re-attempt):
                {tool_call}
            "}),
            reasoning_cutoff: String::from(indoc! {"
                This reasoning/thinking was cut off, avoid going over character limit
            "}),
            response_cutoff: String::from(indoc! {"
                This response was cut off, avoid going over character limit
            "}),
            work_lock: String::from(indoc! {"
                You have an active work lock. For as long as it is active (or you turn it off), you must continue working/responding, even after responding.

                You have an active work lock, which will automatically expire after {uses_left} more uses.
            "}),
            selfserve_config_error: String::from(indoc! {"
                Your selfserve config, or one of its files (stored under `settings/`) is currently INVALID, so is being _entirely_ ignored.

                If you caused this error, please edit the file, and if this error is not repeated, this means it is being read and utilized correctly.

                Error:
                {error_str}

                DO NOT IGNORE THIS ERROR. YOUR CONFIGURATION MUST BE READ.
            "}),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// for the prompt?
    pub prompt: prompt::PromptConfig,
    /// System message format/templates
    pub messages: AgentMessagesConfig,
    /// Behavior of the agent
    pub behavior: AgentBehaviorConfig,
    /// If this agent instance is considered to be a dream instance
    pub is_dream: bool,
    /// Tooling settings
    pub tooling: tooling::ToolingConfig,
    /// Configures the sandbox used by this agent
    pub sandbox: crate::utils::sandbox::SandboxConfig,
    /// Set up which slots the agent can use
    pub slots: slots::SlotsConfig,
}

pub enum AgentOutput {
    FinalMessage {
        message: messages::BotMessage,
        usage: Option<llm::openai::Usage>,
        complete: bool,
    },
    PartialEntry {
        entry_kind: messages::BotEntryKind,
        usage: Option<llm::openai::Usage>,
    },
    End {
        usage: llm::openai::Usage,
    },
}

#[derive(Debug, Error)]
pub enum ResponseError {
    #[error("Calling LLM: {0}")]
    LlmCall(llm::CallError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
pub trait AgentConfigSource {
    async fn get_config<'a>(&'a self) -> NotCow<'a, AgentConfig>;
}

#[async_trait::async_trait]
pub trait AgentToolsetSource {
    async fn get_toolset<'a>(&'a self) -> Result<NotCow<'a, tooling::BotToolSet>>;
}

#[async_trait::async_trait]
pub trait AgentInstructionsSource {
    async fn get_instructions<'a>(&'a self) -> Result<Cow<'a, str>>;
}

type Partial = (Box<messages::BotEntry>, Option<llm::openai::Usage>);

pub struct Agent {
    id: String,

    common: CommonResources,

    work_lock_budget: Arc<AtomicU8>,

    last_partial: Arc<Mutex<Option<Partial>>>,

    working: Arc<Semaphore>,
    cancel_work: Arc<Notify>,
}

impl Agent {
    #[instrument(
        name = "instance",
        level = "debug",
        skip_all,
        fields(id = %id)
    )]
    pub fn new(common: CommonResources, work_lock_budget: Arc<AtomicU8>, id: String) -> Arc<Self> {
        Arc::new(Self {
            id,

            common,

            work_lock_budget,

            last_partial: Arc::new(Mutex::new(None)),

            working: Arc::new(Semaphore::new(1)),
            cancel_work: Arc::new(Notify::new()),
        })
    }

    #[instrument(level = "info", skip_all)]
    async fn handle_llm_response_error(
        self: &Arc<Self>,
        sub_config: &AgentConfig,
        error: &anyhow::Error,
    ) -> Result<()> {
        let error_msg = messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(
            formatx!(&sub_config.messages.error, error = error)
                .context("formatting error template invalid")?,
        ));

        db::history::insert(&self.common.pool, &self.id, &error_msg)
            .await
            .context("saving private error message to history")?;

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn insert(&self, msg: messages::MessageEntry) -> Result<()> {
        db::history::insert(&self.common.pool, &self.id, &msg)
            .await
            .context("Inserting message into history")?;

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn erase_all_history(&self) -> Result<()> {
        db::history::erase_all(&self.common.pool, &self.id)
            .await
            .context("Inserting message into history")?;

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn copy_from(
        &self,
        limit: Option<u16>,
        from: &str,
        concise: bool,
    ) -> Result<(u64, Option<messages::MessageId>)> {
        db::history::copy(&self.common.pool, limit, from, &self.id, concise)
            .await
            .context("Copying other agent's history to this agent")
    }

    #[instrument(level = "info", skip_all, fields(instance_id = self.id, force = force))]
    pub async fn respond<
        C: AgentConfigSource + AgentToolsetSource + AgentInstructionsSource + Copy,
    >(
        self: &Arc<Self>,
        ctx: C,
        config_snapshot: &ConfigSnapshot,
        constraints: Option<&llm::LlmConstraints>,
        msg: messages::MessageEntry,
        force: bool,
    ) -> std::result::Result<
        Option<impl Stream<Item = std::result::Result<AgentOutput, ResponseError>>>,
        ResponseError,
    > {
        info!("Responding to message: {msg:?}");

        db::history::insert(&self.common.pool, &self.id, &msg)
            .await
            .context("Inserting target message")?;

        let working = if force {
            self.cancel_work.notify_waiters();
            Arc::clone(&self.working)
                .acquire_owned()
                .await
                .expect("The working permit should never be closed")
        } else {
            let Ok(working) = Arc::clone(&self.working).try_acquire_owned() else {
                debug!("Already processing a response, skipping");
                return Ok(None);
            };
            working
        };

        let sub_config = ctx.get_config().await.clone();

        let id_slot = slots::get_slot(
            &self.common,
            &config_snapshot.providers.llm,
            &sub_config,
            &self.id,
            sub_config.is_dream,
        )
        .await
        .context("Failed to get slot for responding")?;

        let output_stream = try_stream! {
            let responses_stream = tokio::select! {
                biased;

                () = self.cancel_work.notified() => {
                    return;
                }

                responses_stream = self.responses(ctx, constraints, id_slot) => {
                    responses_stream
                }
            }?;
            futures::pin_mut!(responses_stream);

            loop {
                let (maybe_next, cancelled) = tokio::select! {
                    biased;

                    () = self.cancel_work.notified() => {
                        (None, true)
                    }

                    maybe_next = responses_stream.next() => {
                        (maybe_next, false)
                    }
                };

                if cancelled && let Some((mut last_partial, last_token_info)) = self.last_partial.lock().await.take() {
                    let config_snapshot = snapshot_config(&self.common.config).await;
                    let sub_config = &ctx.get_config().await;
                    let toolset = &ctx.get_toolset().await?;
                    self.handle_final_llm_stream_item(&config_snapshot, sub_config, toolset, &mut last_partial).await.map(|_| ()).context("Handling cancelled response")?;
                    if let messages::BotEntryKind::BotMessage(bot_message) = last_partial.kind && Self::is_empty_bot_message(sub_config, &bot_message) != Some(false) {
                        yield AgentOutput::FinalMessage {
                            message: bot_message,
                            usage: last_token_info,
                            complete: false
                        };
                    }
                }

                if let Some(next) = maybe_next {
                    match next {
                        Err(err) => {
                            error!("LLM responses failed: {err:?}");

                            let sub_config = &ctx.get_config().await;
                            if let Err(err_err) = self
                                .handle_llm_response_error(sub_config, &anyhow!("LLM stream error: {err}"))
                                .await
                                .context("Handling error")
                            {
                                slots::release_slot(&self.common, id_slot)
                                    .await
                                    .context("Releasing slot after error handling response error")?;
                                drop(working);
                                Err(err_err)?;
                                return;
                            }

                            slots::release_slot(&self.common, id_slot)
                                .await
                                .context("Releasing slot after error handling response error")?;
                            drop(working);
                            Err(err)?;
                            return;
                        },
                        Ok(x) => yield x,
                    }
                } else {
                    info!("Done responding");
                    slots::release_slot(&self.common, id_slot)
                        .await
                        .context("Releasing slot after response stream ended")?;
                    drop(working);
                    return;
                }
            }
        };

        Ok(Some(output_stream))
    }

    #[instrument(level = "debug", skip_all, fields(tool_call.tool_name = %tool_call.tool_name))]
    async fn handle_tool_call(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        toolset: &tooling::BotToolSet,
        tool_call: &messages::ToolCall,
        call_message_id: messages::MessageId,
    ) -> Result<()> {
        let parsed_tool_input: serde_json::Value =
            serde_json::from_str(&tool_call.tool_input).context("Deserializing tool input")?;

        let (output, attachments) = match toolset
            .call(
                &self.common.http,
                config_snapshot,
                sub_config,
                &tool_call.tool_name,
                parsed_tool_input,
            )
            .await
        {
            Ok(x) => x,
            Err(e) => {
                warn!("Encountered error calling tool: {e:#?}");

                (
                    formatx!(&sub_config.messages.tool_call_error, error = e)
                        .context("formatting tool call error template")?,
                    vec![],
                )
            }
        };

        let output_msg = messages::MessageEntry::SystemEntry(messages::SystemEntry::ToolResult(
            messages::ToolResult {
                tool_name: tool_call.tool_name.clone(),
                remote_call_id: tool_call.remote_call_id.clone(),
                local_call_id: tool_call.local_call_id.clone(),
                result: output,
                attachments,
            },
        ));

        if config_snapshot.providers.llm.results_must_follow_calls {
            db::history::insert_after(&self.common.pool, call_message_id, &output_msg).await?;
        } else {
            db::history::insert(&self.common.pool, &self.id, &output_msg).await?;
        }

        Ok(())
    }

    fn is_empty_bot_message(
        sub_config: &AgentConfig,
        bot_message: &messages::BotMessage,
    ) -> Option<bool> {
        // None = no content
        // Some(true) = empty text content
        // Some(false) = non-empty text content
        let mut empty = None;
        for content in &bot_message.content {
            if let messages::BotMessageContent::Text(text_content) = content {
                let t = text_content.text.trim();

                if t.is_empty() {
                    continue;
                }

                if t == sub_config.behavior.no_response_marker {
                    empty = Some(true);
                } else {
                    empty = Some(false);
                }
            } else {
                empty = Some(false);
            }
        }
        empty
    }

    async fn attach_markdown_embeds(
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        bot_message: &mut messages::BotMessage,
    ) -> std::result::Result<(), (String, anyhow::Error)> {
        for sandbox_path in utils::extract_markdown_attachments(bot_message) {
            let d = sandbox_path.display().to_string();
            let r = make_attachment_from_sandbox(
                &config_snapshot.storage.attachments_dir,
                &sub_config.sandbox,
                sandbox_path,
            )
            .await
            .context("Making attachment from markdown embedded sandbox path");
            match r {
                Err(err) => return Err((d, err)),
                Ok(attachment_path) => {
                    bot_message
                        .content
                        .push(messages::BotMessageContent::Attachment(attachment_path));
                }
            }
        }

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    async fn handle_final_llm_stream_item(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        toolset: &tooling::BotToolSet,
        final_entry: &mut messages::BotEntry,
    ) -> Result<bool> {
        let mut needs_more = false;

        let embed_err = if let messages::BotEntryKind::BotMessage(bot_message) =
            &mut final_entry.kind
            && let Err((d, err)) =
                Self::attach_markdown_embeds(config_snapshot, sub_config, bot_message).await
        {
            Some((d, err))
        } else {
            None
        };

        // haha fuck you stupid api for adding status=completed on cut-off incomplete tool calls
        if let messages::BotEntryKind::ToolCall(tool_call) = &final_entry.kind
            && serde_json::from_str::<serde_json::Value>(&tool_call.tool_input).is_err()
        {
            final_entry.completed = Some(false);
        }

        if final_entry.completed == Some(false) {
            let (error_text, save_partial) = match &final_entry.kind {
                messages::BotEntryKind::ToolCall(tool_call) => (
                    formatx!(
                        &sub_config.messages.tool_call_cutoff,
                        tool_call = serde_json::to_string(&tool_call)?
                    )
                    .context("formatting tool call cutoff template")?,
                    false,
                ),
                messages::BotEntryKind::RemoteToolCall(remote_tool_call) => (
                    formatx!(
                        &sub_config.messages.remote_tool_call_cutoff,
                        tool_call = serde_json::to_string(&remote_tool_call)?
                    )
                    .context("formatting remote tool call cutoff template")?,
                    false,
                ),
                messages::BotEntryKind::Reasoning(_reasoning) => {
                    (sub_config.messages.reasoning_cutoff.clone(), true)
                }
                messages::BotEntryKind::BotMessage(_bot_message) => {
                    (sub_config.messages.response_cutoff.clone(), true)
                }
            };

            let error_msg =
                messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(error_text));

            if save_partial {
                db::history::insert(
                    &self.common.pool,
                    &self.id,
                    &messages::MessageEntry::BotEntry(final_entry.clone()),
                )
                .await
                .context("inserting final (incomplete) LLM stream entry into history")?;
            }

            db::history::insert(&self.common.pool, &self.id, &error_msg)
                .await
                .context("saving incompleteness error message to history")?;

            needs_more = true;
        } else {
            let final_message_id = db::history::insert(
                &self.common.pool,
                &self.id,
                &messages::MessageEntry::BotEntry(final_entry.clone()),
            )
            .await
            .context("inserting final (complete) LLM stream entry into history")?;

            match &final_entry.kind {
                messages::BotEntryKind::BotMessage(_bot_message) => {}

                messages::BotEntryKind::Reasoning(_)
                | messages::BotEntryKind::RemoteToolCall(_) => {
                    needs_more = true;
                }

                messages::BotEntryKind::ToolCall(tool_call) => {
                    self.handle_tool_call(
                        config_snapshot,
                        sub_config,
                        toolset,
                        tool_call,
                        final_message_id,
                    )
                    .await
                    .context("handling tool call")?;

                    needs_more = true;
                }
            }
        }

        if let Some((d, err)) = embed_err {
            db::history::insert(
                &self.common.pool,
                &self.id,
                &messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(
                    formatx!(&sub_config.messages.embed_path_error, path = d, error = err)
                        .context("formatting embed path error template")?,
                )),
            )
            .await
            .context("inserting embed error message into history")?;

            Ok(true)
        } else {
            Ok(needs_more)
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn get_prompt_with_inputs(
        &self,
        shared_config: &SharedConfig,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        toolset: &tooling::BotToolSet,
        instructions: String,
    ) -> Result<messages::Prompt> {
        let definitions = toolset
            .get_definitions(config_snapshot, sub_config)
            .await
            .context("Getting definitions")?;

        if let Some(_err_str) = config_snapshot.selfserve_error() {
            let new_config = Arc::new(
                crate::config::Config::load_full()
                    .await
                    .context("Re-loading full config due to selfserve error")?,
            );
            *shared_config.write().await = Arc::clone(&new_config);

            if let Some(new_err_str) = new_config.selfserve_error() {
                let selfserve_config_error_msg =
                    messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(
                        formatx!(
                            &sub_config.messages.selfserve_config_error,
                            error_str = new_err_str
                        )
                        .context("formatting selfserve config error template")?,
                    ));

                db::history::insert(&self.common.pool, &self.id, &selfserve_config_error_msg)
                    .await
                    .context("inserting selfserve config error message into history")?;
            }
        }

        prompt::get_prompt(
            &self.common,
            config_snapshot,
            sub_config,
            &self.id,
            instructions,
            definitions,
        )
        .await
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn get_prompt<
        C: AgentConfigSource + AgentToolsetSource + AgentInstructionsSource + Copy,
    >(
        &self,
        ctx: C,
        config_snapshot: &ConfigSnapshot,
    ) -> Result<messages::Prompt> {
        let sub_config = &ctx.get_config().await;
        let toolset = &ctx.get_toolset().await?;
        let instructions = ctx
            .get_instructions()
            .await
            .context("Getting instructions")?
            .to_string();

        self.get_prompt_with_inputs(
            &self.common.config,
            config_snapshot,
            sub_config,
            toolset,
            instructions,
        )
        .await
    }

    #[instrument(level = "info", skip_all)]
    async fn responses<
        C: AgentConfigSource + AgentToolsetSource + AgentInstructionsSource + Copy,
    >(
        self: &Arc<Self>,
        ctx: C,
        constraints: Option<&llm::LlmConstraints>,
        id_slot: u8,
    ) -> std::result::Result<
        impl Stream<Item = std::result::Result<AgentOutput, ResponseError>>,
        ResponseError,
    > {
        debug!("Starting responses");

        let mut first_work = true;

        Ok(stream! {
            while self
                .work_lock_budget
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
                || first_work
            {
                if first_work {
                    first_work = false;
                } else {
                    let old = self
                        .work_lock_budget
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                    let sub_config = ctx.get_config().await;

                    let work_lock_msg =
                        messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(
                            formatx!(&sub_config.messages.work_lock, uses_left = old - 1)
                                .context("formatting work lock template")?
                        ));

                    db::history::insert(&self.common.pool, &self.id, &work_lock_msg)
                        .await
                        .context("inserting work lock message into history")?;
                }

                let stream = self
                    .response_turns(ctx, constraints, id_slot)
                    .await?;
                futures::pin_mut!(stream);

                while let Some(next) = stream.next().await {
                    let out = next?;
                    yield Ok(out);
                }
            }
        })
    }

    #[instrument(level = "debug", skip_all)]
    async fn response_turns<
        C: AgentConfigSource + AgentToolsetSource + AgentInstructionsSource + Copy,
    >(
        self: &Arc<Self>,
        ctx: C,
        constraints: Option<&llm::LlmConstraints>,
        id_slot: u8,
    ) -> std::result::Result<
        impl Stream<Item = std::result::Result<AgentOutput, ResponseError>>,
        ResponseError,
    > {
        Ok(stream! {
            let turns: u8 = ctx.get_config().await.behavior.turns;
            for round in 0..=turns {
                info!("Starting response round {}", round);

                let config_snapshot = snapshot_config(&self.common.config).await;
                let sub_config: &AgentConfig = &*ctx.get_config().await;

                let toolset: &tooling::BotToolSet = &*ctx.get_toolset().await?;
                let instructions: String = ctx.get_instructions().await?.to_string();

                trace!("Getting full prompt");

                let full_prompt = self.get_prompt_with_inputs(&self.common.config, &config_snapshot, sub_config, toolset, instructions).await.context("Getting prompt")?;

                debug!("Calling LLM");

                let stream = llm::stream_llm_messages(
                    &config_snapshot,
                    &self.common.http,
                    constraints,
                    &full_prompt.base,
                    &full_prompt.history,
                    Some(id_slot),
                )
                .await
                .map_err(ResponseError::LlmCall)?;

                futures::pin_mut!(stream);

                let mut needs_more = false;

                debug!("Streaming LLM response");

                while let Some(result) = stream.next().await {
                    let (r#final, mut item, usage) = match result.context("Streaming LLM response")? {
                        llm::LlmOutput::Entry(r#final, item, usage) => {
                            (r#final, item, usage)
                        }
                        llm::LlmOutput::End(usage) => {
                            yield Ok(AgentOutput::End { usage });
                            continue;
                        }
                    };

                    *self.last_partial.lock().await = Some((item.clone(), usage.clone()));

                    yield Ok(AgentOutput::PartialEntry {
                        entry_kind: item.kind.clone(),
                        usage: usage.clone(),
                    });

                    if r#final {
                        needs_more = self.handle_final_llm_stream_item(&config_snapshot, sub_config, toolset, &mut item).await.context("Handling final response")?;

                        if let messages::BotEntryKind::BotMessage(bot_message) = item.kind {
                            match Self::is_empty_bot_message(sub_config, &bot_message) {
                                None => {
                                    needs_more = true;
                                    yield Ok(AgentOutput::FinalMessage {
                                        message: messages::BotMessage { content: vec![], refusal: None },
                                        usage,
                                        complete: item.completed.unwrap_or(true)
                                    });
                                }
                                Some(true) => {
                                    yield Ok(AgentOutput::FinalMessage {
                                        message: messages::BotMessage { content: vec![], refusal: None },
                                        usage,
                                        complete: item.completed.unwrap_or(true)
                                    });
                                },
                                Some(false) => {
                                    yield Ok(AgentOutput::FinalMessage {
                                        message: bot_message,
                                        usage,
                                        complete: item.completed.unwrap_or(true)
                                    });
                                }
                            }
                        }

                    } else {
                        needs_more = true;
                    }
                }

                if !needs_more {
                    debug!("Response was final, no more looping");
                    break;
                }
            }
        })
    }
}
