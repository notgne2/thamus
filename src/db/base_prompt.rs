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

    new_base_prompt: &'a str,
    new_definitions: &'b BTreeMap<String, messages::ToolingDefinition>,
) -> Result<BasePromptData<'a, 'b>> {
    let maybe_base_prompt_r: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT active_base_prompt, practical_base_prompt, active_llm_tools, practical_llm_tools FROM base_prompts WHERE instance_id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("Fetching saved base prompt and tools")?;

    if let Some((
        active_base_prompt,
        practical_base_prompt,
        active_llm_tools_encoded,
        practical_llm_tools_encoded,
    )) = maybe_base_prompt_r
    {
        Ok((
            active_base_prompt.into(),
            practical_base_prompt.into(),
            Cow::Owned(
                serde_json::from_str(&active_llm_tools_encoded)
                    .context("Decoding active LLM tools")?,
            ),
            Cow::Owned(
                serde_json::from_str(&practical_llm_tools_encoded)
                    .context("Decoding practical LLM tools")?,
            ),
        ))
    } else {
        let new_definitions_encoded =
            serde_json::to_string(&new_definitions).context("Encoding LLM tools")?;

        sqlx::query("INSERT INTO base_prompts (instance_id, active_base_prompt, practical_base_prompt, active_llm_tools, practical_llm_tools) VALUES (?1, ?2, ?2, ?3, ?3)")
            .bind(id)
            .bind(new_base_prompt)
            .bind(new_definitions_encoded)
            .execute(pool)
            .await
            .context("Setting initial base prompt in db")?;

        Ok((
            new_base_prompt.into(),
            new_base_prompt.into(),
            Cow::Borrowed(new_definitions),
            Cow::Borrowed(new_definitions),
        ))
    }
}

#[instrument(level = "debug", skip_all)]
pub async fn update_practical_base_prompt(
    pool: &SqlitePool,
    id: &str,
    new_base_prompt: &str,
) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE base_prompts SET practical_base_prompt = ?2 WHERE instance_id = ?1 AND practical_base_prompt IS NOT ?2",
    )
    .bind(id)
    .bind(new_base_prompt)
    .execute(pool)
    .await
    .context("inserting or updating saved base prompt")?;

    Ok(r.rows_affected() > 0)
}

#[instrument(level = "debug", skip_all)]
pub async fn update_practical_definitions(
    pool: &SqlitePool,
    id: &str,
    new_definitions: &BTreeMap<String, messages::ToolingDefinition>,
) -> Result<bool> {
    let new_definitions_encoded =
        serde_json::to_string(new_definitions).context("Encoding tool definitions")?;

    let r = sqlx::query(
        "UPDATE base_prompts SET practical_llm_tools = ?2 WHERE instance_id = ?1 AND practical_llm_tools IS NOT ?2",
    )
    .bind(id)
    .bind(new_definitions_encoded)
    .execute(pool)
    .await
    .context("inserting or updating saved tool definitions")?;

    Ok(r.rows_affected() > 0)
}
