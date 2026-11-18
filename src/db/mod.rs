pub mod base_prompt;
pub mod dream;
pub mod history;
pub mod mem;
pub mod slots;

use anyhow::{Context as _, Result};
use jiff::Timestamp;
use sqlx::SqlitePool;
use tracing::instrument;

pub async fn set_status(pool: &SqlitePool, id: &str, status: &str) -> Result<()> {
    sqlx::query("INSERT INTO statuses (instance_id, status) VALUES (?1, ?2) ON CONFLICT (instance_id) DO UPDATE SET status = ?2")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await
        .context("updating status in database")?;

    Ok(())
}

#[instrument(level = "debug", skip_all)]
pub async fn clear_slots(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE llm_slot_leases SET in_use = 0")
        .execute(pool)
        .await
        .context("Clearing in_use slots in database")?;

    Ok(())
}

#[instrument(level = "debug", skip_all)]
pub async fn get_status(pool: &SqlitePool, id: &str) -> Result<Option<String>> {
    let status_r: Option<(String,)> =
        sqlx::query_as("SELECT status FROM statuses WHERE instance_id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .context("Failed to get saved status")?;

    Ok(status_r.map(|(s,)| s))
}

#[instrument(level = "debug", skip_all)]
pub async fn get_last_response_info(pool: &SqlitePool, id: &str) -> Result<(Timestamp, bool)> {
    let (last_assistant_msg_time, stopped_since_talked): (Option<String>, bool) = sqlx::query_as(
        "
            SELECT
                MAX(created_at) FILTER (
                    WHERE json_valid(history.json)
                    AND json_type(history.json, '$.BotEntry') IS NOT NULL
                ) AS last_assistant_ts,
                COALESCE(
                    MAX(created_at) FILTER (
                        WHERE json_valid(history.json)
                        AND json_type(history.json, '$.UserMessage.content') = 'array'
                        AND EXISTS (
                            SELECT 1 FROM json_each(history.json, '$.UserMessage.content') AS c
                            WHERE json_extract(c.value, '$.Text') = 'secretstop'
                        )
                    ) > MAX(created_at) FILTER (
                        WHERE json_valid(history.json)
                        AND json_type(history.json, '$.BotEntry') IS NOT NULL
                    ),
                    0
                ) AS is_stopped
            FROM history
            WHERE instance_id = ?1;
                    ",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .context("checking recent history response state")?;

    let last_assistant_msg_time = match last_assistant_msg_time {
        Some(ts) => ts
            .parse::<Timestamp>()
            .context("parsing stored created_at timestamp")?,
        None => Timestamp::UNIX_EPOCH,
    };

    Ok((last_assistant_msg_time, stopped_since_talked))
}
