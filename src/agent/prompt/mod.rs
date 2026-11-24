use anyhow::{Context as _, Result};
use indoc::indoc;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::OsStr, fmt::Write as _};
use tracing::{instrument, trace};

use crate::{
    CommonResources,
    agent::AgentConfig,
    config::ConfigSnapshot,
    db, messages,
    utils::sandbox::{self, GetName as _},
};
use formatx::formatx;

pub mod base_prompt;

/// Configure the messages given by the prompt system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptMessagesConfig {
    /// Template for indicating an attachment was saved from a message
    pub attachment: String,
    /// Template for message indicating a tool call in history was missing a result
    pub missing_tool_result: String,
}

impl Default for PromptMessagesConfig {
    fn default() -> Self {
        Self {
            attachment: String::from(indoc! {"
                [Received attachment: ![]({path})]
            "}),
            missing_tool_result: String::from(indoc! {"
                Process restarted before tool reached completion
            "}),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Messages given by prompt system
    pub messages: PromptMessagesConfig,
    /// Configuration for the base-prompt system
    pub base: base_prompt::BasePromptConfig,
}

#[instrument(level = "trace", skip_all)]
pub async fn fix_attachments_for_prompting(
    sub_config: &AgentConfig,
    supported_extensions: &[impl AsRef<str> + Sync],
    ctx: &mut Vec<messages::Message>,
) -> Result<()> {
    for message in ctx {
        let r = match &mut message.entry {
            messages::MessageEntry::BotEntry(messages::BotEntry {
                kind: messages::BotEntryKind::BotMessage(bot_message),
                ..
            }) => {
                bot_message.content.retain(|c| match c {
                    messages::BotMessageContent::Text(_) => true,
                    messages::BotMessageContent::Attachment(_) => false,
                });
                None
            }
            messages::MessageEntry::UserMessage(user_message) => {
                let mut attachment_paths = Vec::new();
                for c in &user_message.content {
                    if let messages::MessageContent::Attachment(a) = c {
                        attachment_paths.push(a.clone());
                    }
                }

                user_message.content.retain(|c| match c {
                    messages::MessageContent::Text(_) => true,
                    messages::MessageContent::Attachment(a) => a.extension().is_some_and(|e| {
                        supported_extensions
                            .iter()
                            .any(|ext| OsStr::new(ext.as_ref()) == e)
                    }),
                });

                let mut m_t = None;
                for x in user_message.content.iter_mut().rev() {
                    if let messages::MessageContent::Text(s) = x {
                        m_t = Some(s);
                    }
                }

                let t = if let Some(t) = m_t {
                    t
                } else {
                    let x = user_message
                        .content
                        .push_mut(messages::MessageContent::Text(String::new()));

                    let messages::MessageContent::Text(t) = x else {
                        unreachable!("bro we just made it be text, it has gotta be text")
                    };

                    t
                };

                Some((t, attachment_paths.clone()))
            }
            messages::MessageEntry::SystemEntry(messages::SystemEntry::ToolResult(tool_result)) => {
                let attachments = tool_result.attachments.clone();

                tool_result.attachments.retain(|a| {
                    a.extension().is_some_and(|e| {
                        supported_extensions
                            .iter()
                            .any(|ext| OsStr::new(ext.as_ref()) == e)
                    })
                });

                Some((&mut tool_result.result, attachments))
            }
            _ => None,
        };

        let Some((text, attachments)) = r else {
            continue;
        };

        // needed
        if attachments.is_empty() {
            continue;
        }

        if !attachments.is_empty() {
            let mut attachment_strs = Vec::new();
            for a in attachments {
                let sandbox_save_path =
                    sandbox::make_sandbox_from_attachment(&sub_config.sandbox, &a)
                        .await
                        .context("Making sandbox from attachment")?;

                let attachment_str = formatx!(
                    &sub_config.prompt.messages.attachment,
                    path = sandbox_save_path.display()
                )
                .context("formatting attachment template invalid")?;

                attachment_strs.push(attachment_str);
            }
            let _ = write!(text, "\n\n{}", attachment_strs.join("\n"));
        }
    }

    Ok(())
}

#[instrument(level = "trace", skip_all)]
pub fn get_missing_tool_call_results(
    sub_config: &AgentConfig,
    ctx: &[messages::Message],
) -> Result<Vec<(usize, messages::MessageId, messages::MessageEntry)>> {
    let mut missing = Vec::new();

    for (i, message) in ctx.iter().enumerate() {
        let messages::MessageEntry::BotEntry(messages::BotEntry {
            kind: messages::BotEntryKind::ToolCall(tool_call),
            ..
        }) = &message.entry
        else {
            continue;
        };

        let mut found_result = false;

        for m in ctx {
            if let messages::MessageEntry::SystemEntry(messages::SystemEntry::ToolResult(
                tool_result,
            )) = &m.entry
                && tool_result.remote_call_id == tool_call.remote_call_id
            {
                found_result = true;
                break;
            }
        }

        if !found_result {
            let result_str = formatx!(
                &sub_config.prompt.messages.missing_tool_result,
                tool_call_id = tool_call.remote_call_id
            )
            .context("formatting missing tool result template")?;

            let msg = messages::MessageEntry::SystemEntry(messages::SystemEntry::ToolResult(
                messages::ToolResult {
                    tool_name: tool_call.tool_name.clone(),
                    remote_call_id: tool_call.remote_call_id.clone(),
                    local_call_id: tool_call.local_call_id.clone(),
                    result: result_str,
                    attachments: vec![],
                },
            ));
            missing.push((i, message.id, msg));
        }
    }

    Ok(missing)
}

#[instrument(level = "debug", skip_all)]
pub async fn get_prompt(
    common: &CommonResources,
    config_snapshot: &ConfigSnapshot,
    sub_config: &AgentConfig,
    id: &str,
    new_instructions: String,
    new_definitions: BTreeMap<String, messages::ToolingDefinition>,
) -> Result<messages::Prompt> {
    trace!("Getting base prompt text");

    let base_prompt = base_prompt::make_cachy_base_prompt(
        sub_config,
        &common.pool,
        id,
        &new_instructions,
        new_definitions,
    )
    .await
    .context("Getting base prompt text (and tools)")?;

    trace!("Getting history");

    let mut history = db::history::get_history(&common.pool, id, None).await?;

    fix_attachments_for_prompting(
        sub_config,
        config_snapshot
            .providers
            .llm
            .supported_file_extensions
            .as_slice(),
        &mut history,
    )
    .await
    .context("Fixing attachments for prompting")?;

    let missing_results = get_missing_tool_call_results(sub_config, &history)
        .context("Getting missing tool call results")?;

    for (i, tool_call_id, missing_tool_result) in missing_results {
        let id = db::history::insert_after(&common.pool, tool_call_id, &missing_tool_result)
            .await
            .context("inserting fabricated missing tool result into history")?;

        // I know nobody is a fan of `insert` on a Vec, but this should never actually happen,
        // it is just here for basically disaster recovery.
        history.insert(
            i + 1,
            messages::Message {
                id,
                entry: missing_tool_result,
            },
        );
    }

    Ok(messages::Prompt {
        base: base_prompt,
        history,
    })
}
