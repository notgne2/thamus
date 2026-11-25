use std::{borrow::Cow, collections::BTreeMap};

use anyhow::{Context as _, Result};
use sqlx::SqlitePool;
use tracing::instrument;

use crate::messages;

pub type BasePromptData<'a, 'b> = (
    Cow<'a, str>,
    Cow<'a, str>,
    Cow<'b, BTreeMap<String, messages::ToolingDefinition>>,
    Cow<'b, BTreeMap<String, messages::ToolingDefinition>>,
);

#[instrument(level = "trace", skip_all)]
pub async fn get_or_insert_base_prompt<'a, 'b>(
    pool: &SqlitePool,
    id: &str,

    new_instructions: &'a str,
    new_tool_definitions: &'b BTreeMap<String, messages::ToolingDefinition>,
) -> Result<BasePromptData<'a, 'b>> {
    let maybe_base_prompt_r: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT active_instructions, practical_instructions, active_tool_definitions, practical_tool_definitions FROM base_prompts WHERE instance_id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("Fetching saved base prompt and tools")?;

    if let Some((
        active_instructions,
        practical_instructions,
        active_tool_definitions_encoded,
        practical_tool_definitions_encoded,
    )) = maybe_base_prompt_r
    {
        Ok((
            active_instructions.into(),
            practical_instructions.into(),
            Cow::Owned(
                serde_json::from_str(&active_tool_definitions_encoded)
                    .context("Decoding active LLM tools")?,
            ),
            Cow::Owned(
                serde_json::from_str(&practical_tool_definitions_encoded)
                    .context("Decoding practical LLM tools")?,
            ),
        ))
    } else {
        let new_definitions_encoded =
            serde_json::to_string(&new_tool_definitions).context("Encoding LLM tools")?;

        sqlx::query("INSERT INTO base_prompts (instance_id, active_instructions, practical_instructions, active_tool_definitions, practical_tool_definitions) VALUES (?1, ?2, ?2, ?3, ?3)")
            .bind(id)
            .bind(new_instructions)
            .bind(new_definitions_encoded)
            .execute(pool)
            .await
            .context("Setting initial base prompt in db")?;

        Ok((
            new_instructions.into(),
            new_instructions.into(),
            Cow::Borrowed(new_tool_definitions),
            Cow::Borrowed(new_tool_definitions),
        ))
    }
}

#[instrument(level = "debug", skip_all)]
pub async fn update_practical_instructions(
    pool: &SqlitePool,
    id: &str,
    new_instructions: &str,
) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE base_prompts SET practical_instructions = ?2 WHERE instance_id = ?1 AND practical_instructions IS NOT ?2",
    )
    .bind(id)
    .bind(new_instructions)
    .execute(pool)
    .await
    .context("inserting or updating saved instructions")?;

    Ok(r.rows_affected() > 0)
}

#[instrument(level = "debug", skip_all)]
pub async fn update_practical_tool_definitions(
    pool: &SqlitePool,
    id: &str,
    new_tool_definitions: &BTreeMap<String, messages::ToolingDefinition>,
) -> Result<bool> {
    let new_tool_definitions_encoded =
        serde_json::to_string(new_tool_definitions).context("Encoding tool definitions")?;

    let r = sqlx::query(
        "UPDATE base_prompts SET practical_tool_definitions = ?2 WHERE instance_id = ?1 AND practical_tool_definitions IS NOT ?2",
    )
    .bind(id)
    .bind(new_tool_definitions_encoded)
    .execute(pool)
    .await
    .context("inserting or updating saved tool definitions")?;

    Ok(r.rows_affected() > 0)
}
