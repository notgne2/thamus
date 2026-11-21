use std::borrow::Cow;
use std::sync::Arc;

use std::sync::atomic::AtomicU8;

use anyhow::{Context as _, Result, bail, ensure};
use futures::stream::StreamExt;

use serde::{Deserialize, Serialize};

use tracing::{debug, instrument, trace};

use crate::CommonResources;
use crate::agent;
use crate::config::ConfigSnapshot;
use crate::db;
use crate::messages;
use crate::providers::llm;
use crate::utils::NotCow;
use crate::utils::tooling;
use formatx::formatx;

/// Configure what triggers a dream to start
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DreamTriggerConfig {
    /// Trigger if this many dreamable history items are present
    pub history_count: Option<usize>,
    /// Trigger if context is at least this full (percent as 0.0-1.0)
    pub context_usage: Option<f64>,
}

/// Configuration for dreaming
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DreamingConfig {
    /// How many messages to preserve NOT in dreams
    pub dream_ago: usize,

    /// When the dream can naturally trigger without being forced
    pub dream_trigger: DreamTriggerConfig,

    /// LLM constraints for during the saving step
    pub saving_constraints: Option<llm::LlmConstraints>,
    /// LLM constraints for during the compaction step
    pub compaction_constraints: Option<llm::LlmConstraints>,

    /// Prompts to use for the dream
    pub messages: DreamPromptsConfig,
    /// Messages to use for the dream output
    pub output_messages: DreamMessages,

    /// Base intro/instructions for the dream describing the process
    pub instructions: String,
}

impl Default for DreamingConfig {
    fn default() -> Self {
        Self {
            dream_ago: 50,
            dream_trigger: DreamTriggerConfig::default(),
            saving_constraints: None,
            compaction_constraints: None,
            messages: DreamPromptsConfig::default(),
            output_messages: DreamMessages::default(),
            instructions: String::from(indoc::indoc! {"
                # You are now in dream mode

                This is a SYSTEM STATE which is DISCONNECTED FROM ALL USERS. You are being invoked by your own system, which collects your answers for dream handling and DOES NOT send them to any users or platforms.

                Your dream follows this sequence: re_compaction -> saving -> compaction. More information about the state will be provided when/if you are entered into it.

                DREAM_STATE = RE_COMPACTION: Shorten your previous compaction/summary
                DREAM_STATE = SAVING: Search for existing saves, clean up saves, updating existing saves or create new ones with new information learnt from chat, repeat until done, return \"DONE\" when done
                DREAM_STATE = COMPACTION: Summarize the the CONVERSATION and NOT things that were saved. This compaction's only purpose is to make the conversation able to continue.

                DO NOT ATTEMPT TO RESPOND TO USERS. YOU ARE NOT CHATTING RIGHT NOW, YOU ARE IN A DREAM STATE. NO USERS ARE PRESENT.

                You MUST NOT add ANY prefixes or suffixes to responses. Respond ONLY with the information requested, or YOU WILL FAIL. There is NOBODY around to read your words, only the dream management system, which is automation, not a human or LLM.

                YOU MUST NOT WRITE A RESPONSE. YOU MUST OUTPUT WHAT IS REQUIRED DEPENDING ON YOUR DREAM STATE

                NO users are present. You CANNOT write a response. Responding WILL NOT WORK. You CANNOT finish responding. You are NOT in a talking state.

                You ARE dreaming. You MUST adhere to the SPECIFIED RESPONSE FORMAT. Responding incorrectly WILL break your system. You are NOT allowed to violate your dream state formatting.

                Adhere to further dream information provided by system.
            "}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DreamMessages {
    /// Template for wrapping the final generated compaction
    pub compaction: String,
}

impl Default for DreamMessages {
    fn default() -> Self {
        Self {
            compaction: String::from(indoc::indoc! {"
                Highly compressed of old/previous compaction/summary:
                {re_compaction_str}

                Current compaction/summary:
                {final_str}
            "}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DreamPromptsConfig {
    /// Announces and describes the re-compaction step
    pub re_compaction: String,
    /// Announces and describes the saving step
    pub saving: String,
    /// Announces and describes the compaction step
    pub compaction: String,
}

impl Default for DreamPromptsConfig {
    fn default() -> Self {
        Self {
            re_compaction: String::from(indoc::indoc! {"
                # You are now in the DREAM_STATE = RE_COMPACTION dream state.

                Output a much smaller version of your previous summary. It is too long. Your new summary must be only 20% of the size AT MOST.

                You are compacting/summarizing the PREVIOUS SUMMARY. NOT any chat messages or anything current. Make it MUCH smaller, even if losing information.

                Output ONLY the new rewritten summary, without any prefixes or suffixes.
            "}),
            saving: String::from(indoc::indoc! {"
                # You are now in the DREAM_STATE = SAVING dream state.

                You are NOT writing a compaction or summary. Your job is to use your search and save tools to make sure information is appropriately saved.

                Save everything you should have saved (but forgot to), searching for EVERY SINGLE ONE in case it was saved by an external source.

                Clean up your saves (i.e. duplicates, outdated saves)

                When done with EVERYTHING, return done \"DONE\", but once you have returned \"DONE\", the system will advance you to compaction - no more saving will be possible.

                DO NOT change saves based off of ANYTHING except the chat history itself. You must ONLY save things which you have learnt in the chat, and forgotten to save while chatting. DO NOT save information from saves or external sources.

                Recognized responses: \"DONE\" when done, \"CONTINUE\" when more saving is needed
            "}),
            compaction: String::from(indoc::indoc! {"
                # You are now in the DREAM_STATE = COMPACTION dream state.

                Your ONLY job is to summarize the things that were NOT saved (either in your last dream state, or previously), as concise and compact as possible, outputting this summary for the purpose of allowing conversation to continue smoothly.

                ONLY summarize things that are EXCLUSIVE to the chat history, NEVER any information that is saved, or was fetched from an external source. Other information will already be retained.

                Be EXTREMELY brief and concise, packing as much information as possible into a single, concise summary.

                Include ALL information that is ONLY in the chat. Include NO information that could be fetched from saves or an external source.

                This should be a COMPACT SUMMARY, not a complete chat log recollection. You should retain chronology, but DO NOT RETAIN INDIVIDUAL MESSAGES

                Recognized responses: The compaction/summary requested, with no prefixes or suffixes. NEVER write anything other than the full compaction
            "}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamState {
    ReCompaction,
    Saving,
    Compaction,
}
impl std::fmt::Display for DreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReCompaction => write!(f, "re-compaction"),
            Self::Saving => write!(f, "saving"),
            Self::Compaction => write!(f, "compaction"),
        }
    }
}

#[instrument(level = "trace", skip_all)]
async fn estimate_dream_overhead(config: &ConfigSnapshot) -> (u64, String) {
    let mut n = 0;

    let saving_constraints = config
        .instance
        .smarts
        .dreaming
        .saving_constraints
        .as_ref()
        .unwrap_or(&config.providers.llm.default_constraints);
    let compaction_constraints = config
        .instance
        .smarts
        .dreaming
        .compaction_constraints
        .as_ref()
        .unwrap_or(&config.providers.llm.default_constraints);

    // 16 turns or so, 2 times assuming we get a CONTINUE
    n += u64::from(saving_constraints.max_gen) * (16 * 2);
    // compaction, once for re-compaction, once for compaction, both reasoning and the compaction itself
    n += u64::from(compaction_constraints.max_gen) * (2 * 2);

    (
        n,
        [
            // always the intro prompt, behavior, and saving instructions
            config.instance.prompts.intro.as_str(),
            config.instance.prompts.behavior.as_str(),
            config.instance.prompts.saving.as_str(),
            // dreams always have dream instructions
            config.instance.smarts.dreaming.instructions.as_str(),
            // the re-compaction
            config
                .instance
                .smarts
                .dreaming
                .messages
                .re_compaction
                .as_str(),
            // assuming 2 CONTINUEs
            config.instance.smarts.dreaming.messages.saving.as_str(),
            config.instance.smarts.dreaming.messages.saving.as_str(),
            // the compaction
            config.instance.smarts.dreaming.messages.compaction.as_str(),
        ]
        .join("\n"),
    )
}

fn make_dream_toolset(common: &CommonResources, saving: bool) -> tooling::BotToolSet {
    if saving {
        let tools: Vec<Box<dyn tooling::BotTool>> = vec![
            Box::new(crate::instance::tools_def::memory::SearchMemory(
                common.clone(),
            )),
            Box::new(crate::instance::tools_def::memory::SaveMemory(
                common.clone(),
            )),
            Box::new(crate::instance::tools_def::memory::DeleteMemory(
                common.clone(),
            )),
        ];

        tooling::BotToolSet::create(false, tools)
    } else {
        tooling::BotToolSet::create(false, vec![])
    }
}

#[instrument(level = "debug", skip_all)]
async fn fit_history(
    config_snapshot: &ConfigSnapshot,
    client: &reqwest::Client,
    max_iterations: usize,
    overhead: u64,
    base_prompt: &messages::BasePrompt,
    history: &[messages::Message],
) -> Result<u16> {
    let max_ctx = u64::from(config_snapshot.providers.llm.max_ctx);

    let hl: u16 = u16::try_from(history.len()).context("History should not be this large")?;

    let t = llm::input_tokens(config_snapshot, client, base_prompt, history)
        .await
        .context("Counting full tokens")?;
    if (t as u64 + overhead) <= max_ctx {
        return Ok(hl);
    }

    let mut lo: u16 = 0;
    let mut hi: u16 = hl;
    let mut iterations = 1;

    while hi - lo > 1 {
        if iterations >= max_iterations {
            break;
        }
        iterations += 1;

        let mid = lo + (hi - lo) / 2;
        ensure!(mid < hl, "midpoint {mid} is out of bounds");

        let mut to_mid = history
            .get(..mid as usize)
            .context("no messages up to midpoint?")?;
        let last_to_mid_message = to_mid.last().context("no messages up to midpoint?")?;

        match &last_to_mid_message.entry {
            messages::MessageEntry::UserMessage(_)
            | messages::MessageEntry::SystemEntry(_)
            | messages::MessageEntry::Error(_) => {}

            messages::MessageEntry::BotEntry(last_bot_entry) => match &last_bot_entry.kind {
                messages::BotEntryKind::BotMessage(_)
                | messages::BotEntryKind::Reasoning(_)
                | messages::BotEntryKind::RemoteToolCall(_) => {}
                messages::BotEntryKind::ToolCall(_tool_call) => {
                    to_mid = history
                        .get(..(mid as usize) - 1)
                        .context("to_mid contained only a single tool call")?;
                }
            },
        }

        let t = llm::input_tokens(config_snapshot, client, base_prompt, to_mid)
            .await
            .context("Counting tokens to midpoint")?;

        if (t as u64 + overhead) <= max_ctx {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    Ok(lo)
}

pub struct DreamContext {
    common: CommonResources,

    // only keeping snapshot because this isn't allowed to change in dreams anyway
    config_snapshot: ConfigSnapshot,

    agent_config: agent::AgentConfig,

    saving: bool,
}

#[async_trait::async_trait]
impl agent::AgentConfigSource for &DreamContext {
    async fn get_config(&self) -> NotCow<'_, agent::AgentConfig> {
        NotCow::Borrowed(&self.agent_config)
    }
}

#[async_trait::async_trait]
impl agent::AgentToolsetSource for &DreamContext {
    async fn get_toolset(&'_ self) -> Result<NotCow<'_, tooling::BotToolSet>> {
        Ok(NotCow::Owned(make_dream_toolset(&self.common, self.saving)))
    }
}

#[async_trait::async_trait]
impl agent::AgentInstructionsSource for &DreamContext {
    async fn get_instructions(&'_ self) -> Result<Cow<'_, str>> {
        let q = &self.config_snapshot.instance.prompts;

        Ok(Cow::Owned(format!(
            "{}\n\n{}\n\n{}\n\n{}",
            q.intro,
            q.saving,
            q.behavior,
            self.config_snapshot.instance.smarts.dreaming.instructions
        )))
    }
}

#[instrument(level = "debug", skip(common, config_snapshot))]
pub async fn prep_if_should_dream(
    common: &CommonResources,
    config_snapshot: &ConfigSnapshot,
    instance_id: &str,
    // the presence of this size implies this is NOT a forced dream,
    // without it, we do not check triggers, and always dream
    context_size: Option<u32>,
) -> Result<Option<messages::MessageId>> {
    let dream_context = DreamContext {
        common: common.clone(),
        config_snapshot: Arc::clone(config_snapshot),
        agent_config: agent::AgentConfig {
            is_dream: true,
            ..config_snapshot.default_agent().clone()
        },
        saving: true,
    };

    let agent = agent::Agent::new(
        common.clone(),
        Arc::new(AtomicU8::new(0)),
        format!("{instance_id}_dream"),
    );
    agent
        .erase_all_history()
        .await
        .context("Erasing old dream history")?;
    let (original_len, Some(last_original_message_id)) = agent
        .copy_from(None, instance_id, true)
        .await
        .context("Copying original history to dream")?
    else {
        bail!("Original instance history has no entries?");
    };

    let mut prompt = Box::pin(agent.get_prompt(&dream_context, config_snapshot))
        .await
        .context("Getting dream prompt")?;

    if let Some(context_size) = context_size {
        let dreamable_len = prompt
            .history
            .len()
            .saturating_sub(config_snapshot.instance.smarts.dreaming.dream_ago);

        let dream_trigger = &config_snapshot.instance.smarts.dreaming.dream_trigger;

        debug!(
            "{dreamable_len}/{:?} dreamable messages, {}%/{:?}% context",
            dream_trigger.history_count,
            (f64::from(context_size) / f64::from(config_snapshot.providers.llm.max_ctx)) * 100.,
            dream_trigger.context_usage.map(|x| x * 100.)
        );

        let enough_dream_history = dream_trigger
            .history_count
            .is_some_and(|count| dreamable_len >= count);

        let enough_dream_context = dream_trigger.context_usage.is_some_and(|usage| {
            (f64::from(config_snapshot.providers.llm.max_ctx) * usage) <= f64::from(context_size)
        });

        if !enough_dream_history && !enough_dream_context {
            trace!("Dream triggers not satisfied, and not forced - skipping");
            return Ok(None);
        }

        prompt.history.truncate(dreamable_len);
    }

    let (token_overhead, token_overhead_str) = estimate_dream_overhead(config_snapshot).await;
    prompt.base.instructions = token_overhead_str;
    let fit = fit_history(
        config_snapshot,
        &common.http,
        8,
        token_overhead,
        &prompt.base,
        &prompt.history,
    )
    .await
    .context("Fitting history for dream")?;

    if (u64::from(fit)) < original_len {
        Box::pin(async {
            agent
                .erase_all_history()
                .await
                .context("Erasing old dream history")?;
            let (_entries_copied, Some(last_original_message_id)) = agent
                .copy_from(Some(fit), instance_id, true)
                .await
                .context("Copying original history to dream")?
            else {
                bail!("Original instance history has no entries?");
            };
            debug!("Dreaming only {fit} entries from original history");
            Ok(Some(last_original_message_id))
        })
        .await
    } else {
        debug!("A whole dream is possible");
        Ok(Some(last_original_message_id))
    }
}

#[instrument(level = "info", skip(common, config_snapshot, update_callback))]
pub async fn run_dream<F>(
    common: CommonResources,
    config_snapshot: &ConfigSnapshot,

    instance_id: &str,
    insert_compaction_after: messages::MessageId,

    update_callback: F,
) -> Result<()>
where
    F: Fn(DreamState, u16),
{
    let agent = agent::Agent::new(
        common.clone(),
        Arc::new(AtomicU8::new(0)),
        format!("{instance_id}_dream"),
    );

    let mut dream_context = DreamContext {
        common: common.clone(),
        config_snapshot: Arc::clone(config_snapshot),
        agent_config: agent::AgentConfig {
            is_dream: true,
            ..config_snapshot.default_agent().clone()
        },
        saving: false,
    };

    let mut active_dream_state = Some(DreamState::ReCompaction);
    let mut re_compaction: Option<String> = None;
    while let Some(dream_state) = active_dream_state {
        dream_context.saving = dream_state == DreamState::Saving;
        update_callback(dream_state, 0);

        let constraints = if dream_context.saving {
            &config_snapshot.instance.smarts.dreaming.saving_constraints
        } else {
            &config_snapshot
                .instance
                .smarts
                .dreaming
                .compaction_constraints
        };

        let dream_state_blurb = match dream_state {
            DreamState::ReCompaction => {
                &config_snapshot
                    .instance
                    .smarts
                    .dreaming
                    .messages
                    .re_compaction
            }
            DreamState::Saving => &config_snapshot.instance.smarts.dreaming.messages.saving,
            DreamState::Compaction => &config_snapshot.instance.smarts.dreaming.messages.compaction,
        };

        let dream_state_msg = messages::MessageEntry::SystemEntry(messages::SystemEntry::Misc(
            dream_state_blurb.clone(),
        ));

        let s = Box::pin(agent.respond(
            &dream_context,
            config_snapshot,
            constraints.as_ref(),
            dream_state_msg,
            true,
        ))
        .await
        .context("Responding in dream")?
        .context("Dream responses should be forced")?;
        tokio::pin!(s);

        while let Some(r) = s.next().await {
            match r.context("Receiving response chunk for dream")? {
                crate::instance::agent::AgentOutput::FinalMessage { message, .. } => {
                    let final_str = message
                        .content
                        .into_iter()
                        .filter_map(|content| match content {
                            messages::BotMessageContent::Text(text) => Some(text.text),
                            messages::BotMessageContent::Attachment(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");

                    active_dream_state = match dream_state {
                        DreamState::ReCompaction => {
                            re_compaction = Some(final_str.clone());
                            Some(DreamState::Saving)
                        }
                        DreamState::Compaction => {
                            let compaction_msg = formatx!(
                                &config_snapshot
                                    .instance
                                    .smarts
                                    .dreaming
                                    .output_messages
                                    .compaction,
                                final_str = final_str,
                                re_compaction_str = re_compaction
                                    .as_deref()
                                    .unwrap_or("[no re-compaction produced]"),
                            )
                            .context("formatting compaction template")?;

                            db::dream::save_compaction(
                                &common.pool,
                                insert_compaction_after,
                                compaction_msg,
                            )
                            .await
                            .context("saving compaction")?;

                            None
                        }
                        DreamState::Saving => {
                            if final_str.to_uppercase() == "DONE" {
                                Some(DreamState::Compaction)
                            } else {
                                Some(DreamState::Saving)
                            }
                        }
                    };

                    debug!(
                        "Done with all turns for {dream_state}, and have output of {final_str} - starting {active_dream_state:?}"
                    );
                }
                crate::instance::agent::AgentOutput::PartialEntry { usage, .. } => {
                    let Some(usage) = usage else {
                        continue;
                    };

                    let def_constraints = constraints
                        .as_ref()
                        .unwrap_or(&config_snapshot.providers.llm.default_constraints);

                    update_callback(
                        dream_state,
                        u16::try_from((usage.tokens_generated * 100) / def_constraints.max_gen)
                            .unwrap_or(u16::MAX),
                    );
                }
                crate::instance::agent::AgentOutput::End { .. } => {}
            }
        }
    }

    Ok(())
}
