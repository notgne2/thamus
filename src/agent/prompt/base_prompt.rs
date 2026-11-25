use std::collections::BTreeMap;

use crate::agent::{AgentConfig, utils as agent_utils};
use crate::{db, messages};
use anyhow::{Context as _, Result};
use formatx::formatx;
use indoc::indoc;
use serde::{Deserialize, Serialize};
use tracing::{instrument, trace};

/// Configure the messages given by the base-prompt system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BasePromptMessagesConfig {
    /// Template for when the instructions have changed
    pub instructions_diff: String,
    /// Template for when the tool definitions have changed
    pub definitions_diff: String,
}

impl Default for BasePromptMessagesConfig {
    fn default() -> Self {
        Self {
            instructions_diff: String::from(indoc! {"
                System instructions were updated. You are being shown a diff to avoid invalidating your prompt cache. Real prompt will be updated after the next dream/compaction.

                {diff}
            "}),
            definitions_diff: String::from(indoc! {"
                Tool definitions were updated. You are being shown a diff so can stay up to date without invalidating your prompt cache. Real tools list will be updated after the next dream/compaction.

                {diff}
            "}),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BasePromptConfig {
    /// Configure messages used in the base-prompt system
    pub messages: BasePromptMessagesConfig,
}

#[instrument(level = "debug", skip_all)]
pub async fn make_cachy_base_prompt(
    sub_config: &AgentConfig,
    pool: &sqlx::SqlitePool,

    id: &str,

    new_instructions: &str,
    new_tool_definitions: BTreeMap<String, messages::ToolingDefinition>,
) -> Result<messages::BasePrompt> {
    trace!("Fetching saved prompt information from db");

    let (
        active_instructions,
        practical_instructions,
        active_tool_definitions,
        practical_tool_definitions,
    ) = db::base_prompt::get_or_insert_base_prompt(
        pool,
        id,
        new_instructions,
        &new_tool_definitions,
    )
    .await?;

    trace!("Comparing practical and new instructions");

    if practical_instructions != new_instructions {
        trace!("Practical instructions are not equal to new instructions, updating in db");

        let really_updated =
            db::base_prompt::update_practical_instructions(pool, id, new_instructions)
                .await
                .context("updating practical instructions in db")?;

        if really_updated {
            trace!("Practical instructions were updated, making diff message");

            let diff =
                agent_utils::simple_diff::simple_diff(&practical_instructions, new_instructions);

            let diff_msg = {
                let rendered_diff_template = formatx!(
                    &sub_config.prompt.base.messages.instructions_diff.clone(),
                    diff = diff
                )
                .context("prompt diff template invalid")?;
                messages::MessageEntry::SystemEntry(messages::SystemEntry::InstructionDiff(
                    rendered_diff_template,
                ))
            };

            db::history::insert(pool, id, &diff_msg)
                .await
                .context("inserting diff message into history")?;
        }
    }

    trace!("Comparing practical and new tool definitions");

    let tool_definitions_diff = agent_utils::json_diff::json_diff(
        &serde_json::to_value(practical_tool_definitions)
            .context("Serializing practical definitions to JSON value")?,
        &serde_json::to_value(&new_tool_definitions)
            .context("Serializing new definitions to JSON value")?,
        &[&[None, Some("tools")]],
    );

    if !tool_definitions_diff.trim().is_empty() {
        trace!("Tool definitions diff not empty, updating practical tool definitions in db");

        let really_updated =
            db::base_prompt::update_practical_tool_definitions(pool, id, &new_tool_definitions)
                .await
                .context("updating practical definitions in db")?;

        if really_updated {
            trace!("Practical tool definitions updated, making diff message");

            let diff_msg = {
                let rendered_diff_template = formatx!(
                    &sub_config.prompt.base.messages.definitions_diff.clone(),
                    diff = tool_definitions_diff,
                    format_hint = agent_utils::json_diff::FORMAT_HINT,
                )
                .context("definitions diff template invalid")?;
                messages::MessageEntry::SystemEntry(messages::SystemEntry::DefinitionsDiff(
                    rendered_diff_template,
                ))
            };

            db::history::insert(pool, id, &diff_msg)
                .await
                .context("inserting diff message into history")?;
        }
    }

    Ok(messages::BasePrompt {
        instructions: active_instructions.into(),
        shown_tool_definitions: active_tool_definitions.into_owned(),
        real_tool_definitions: new_tool_definitions,
    })
}
