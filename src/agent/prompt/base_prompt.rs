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
    new_definitions: BTreeMap<String, messages::ToolingDefinition>,
) -> Result<messages::BasePrompt> {
    trace!("Fetching base prompt details from db");

    trace!("Getting new base prompt");

    let (active_instructions, practical_instructions, active_definitions, practical_definitions) =
        db::base_prompt::get_or_insert_base_prompt(pool, id, new_instructions, &new_definitions)
            .await?;

    trace!("Comparing practical and new base prompt");

    if practical_instructions != new_instructions {
        trace!(
            "Practical base prompt is not equal to new base prompt, running update of practical base prompt in db"
        );

        let really_updated =
            db::base_prompt::update_practical_base_prompt(pool, id, new_instructions)
                .await
                .context("updating practical base prompt in db")?;

        if really_updated {
            trace!(
                "Practical base prompt was really updated, so inserting prompt diff message into history"
            );

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

            trace!("Inserting prompt diff into history and updating practical prompt in db");

            db::history::insert(pool, id, &diff_msg)
                .await
                .context("inserting diff message into history")?;
        }
    }

    trace!("Comparing practical definitions");

    let definitions_diff = agent_utils::json_diff::json_diff(
        &serde_json::to_value(practical_definitions)
            .context("Serializing practical definitions to JSON value")?,
        &serde_json::to_value(&new_definitions)
            .context("Serializing new definitions to JSON value")?,
        &[&[None, Some("tools")]],
    );

    if !definitions_diff.trim().is_empty() {
        trace!("Definitions diff not empty, running update of practical definitions in db");

        let really_updated =
            db::base_prompt::update_practical_definitions(pool, id, &new_definitions)
                .await
                .context("updating practical definitions in db")?;

        if really_updated {
            trace!(
                "Practical definitions really updated, inserting practical definition diff message"
            );

            let diff_msg = {
                let rendered_diff_template = formatx!(
                    &sub_config.prompt.base.messages.definitions_diff.clone(),
                    diff = definitions_diff,
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

    trace!("Done making base prompt and definitions");

    // Ok((active_instructions.into(), active_definitions))
    Ok(messages::BasePrompt {
        instructions: active_instructions.into(),
        shown_definitions: active_definitions.into_owned(),
        real_definitions: new_definitions,
    })
}
