use crate::messages::{self, MessageId};
use anyhow::{Context as _, Result};
use sqlx::SqlitePool;
use tracing::instrument;

#[instrument(level = "debug", skip_all)]
pub async fn save_compaction(
    pool: &SqlitePool,
    insert_after: MessageId,
    text: String,
) -> Result<()> {
    let compaction_message =
        messages::MessageEntry::SystemEntry(messages::SystemEntry::Compaction(text));

    super::history::insert_after(pool, insert_after, &compaction_message).await?;

    sqlx::query(
        "
        UPDATE base_prompts
        SET
            active_base_prompt = practical_base_prompt,
            active_llm_tools = practical_llm_tools
        WHERE instance_id = (SELECT instance_id FROM history WHERE id = ?1 LIMIT 1);
        ",
    )
    .bind(insert_after)
    .execute(pool)
    .await
    .context("inserting or updating saved base prompt")?;

    Ok(())
}
