use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlitePool};
use tracing::{debug, instrument, trace};

use crate::messages;

#[instrument(level = "debug", skip_all, fields(id))]
pub async fn erase_all(pool: &SqlitePool, id: &str) -> Result<u64> {
    let result = sqlx::query("DELETE FROM history WHERE instance_id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .context("Wiping out history")?;

    sqlx::query("DELETE FROM base_prompts WHERE instance_id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .context("Wiping out base prompts")?;

    Ok(result.rows_affected())
}

#[instrument(level = "debug", skip_all)]
pub async fn insert_after(
    pool: &SqlitePool,
    insert_after: messages::MessageId,
    message: &messages::MessageEntry,
) -> Result<i32> {
    let json_val = serde_json::to_value(message)
        .context("serializing compaction message entry to history JSON")?;

    let (id,): (i32,) = sqlx::query_as(
        "
            INSERT INTO history (instance_id, json, created_at, sorting)
            SELECT target_entry.instance_id, ?1, target_entry.created_at, target_entry.sorting + 1
            FROM (
                SELECT instance_id, created_at, sorting
                FROM history
                WHERE id = ?2
                LIMIT 1
            ) AS target_entry
            RETURNING id;
        ",
    )
    .bind(json_val)
    .bind(insert_after)
    .fetch_one(pool)
    .await
    .context("inserting message into history at specific point")?;

    Ok(id)
}

const GET_HISTORY_SELECT: &str = r"
WITH latest_compaction AS (
      SELECT created_at, sorting
      FROM history
      WHERE instance_id = ?1
        -- json_valid is required: `json` is plain TEXT, so one malformed
        -- value aborts the entire query. jsonb guaranteed validity for us.
        AND json_valid(json)
        AND json_type(json, '$.SystemEntry.Compaction') IS NOT NULL
      ORDER BY created_at DESC, sorting DESC
      LIMIT 1
    )
SELECT id, json, created_at, sorting
FROM history
WHERE instance_id = ?1
AND (
-- No compaction: return everything
(SELECT created_at FROM latest_compaction) IS NULL
OR
-- Everything after including compaction (lexicographic comparison, since sorting is only a tie-breaker within the same created_at)
(created_at, sorting) >= ((SELECT created_at FROM latest_compaction), (SELECT sorting FROM latest_compaction))
)
ORDER BY created_at ASC, sorting ASC
";

#[instrument(level = "trace", skip_all)]
pub async fn get_history(
    pool: &SqlitePool,
    id: &str,
    limit: Option<u16>,
) -> Result<Vec<messages::Message>> {
    debug!("Fetching conversation history from database");

    let history_rows: Vec<(
        messages::MessageId,
        sqlx::types::Json<messages::MessageEntry>,
    )> = sqlx::query_as(AssertSqlSafe(format!("SELECT id, json FROM (SELECT id, json, created_at, sorting FROM ({GET_HISTORY_SELECT}) ORDER BY created_at DESC, sorting DESC LIMIT ?2) ORDER BY created_at ASC, sorting ASC")))
        .bind(id)
        .bind(i64::from(limit.unwrap_or(1000).min(i16::MAX as u16)))
        .fetch_all(pool)
        .await
        .context("fetching conversation history for LLM response")?;

    let history = history_rows
        .into_iter()
        .map(|(id, x)| messages::Message { id, entry: x.0 })
        .collect::<Vec<messages::Message>>();

    Ok(history)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DumpMessage {
    pub id: messages::MessageId,
    pub instance_id: String,
    pub entry: messages::MessageEntry,
}

#[instrument(level = "trace", skip_all)]
pub async fn dump_all_history(pool: &SqlitePool, limit: Option<u16>) -> Result<Vec<DumpMessage>> {
    let history_rows: Vec<(
        messages::MessageId,
        String,
        sqlx::types::Json<messages::MessageEntry>,
    )> = sqlx::query_as(
        r"
        SELECT id, instance_id, json, created_at, sorting
        FROM (
            SELECT id, instance_id, json, created_at, sorting
            FROM history
            ORDER BY created_at DESC, sorting DESC
            LIMIT ?1
        )
        ORDER BY created_at ASC, sorting ASC;
        ",
    )
    .bind(i64::from(limit.unwrap_or(1000).min(i16::MAX as u16)))
    .fetch_all(pool)
    .await
    .context("fetching conversation history for LLM response")?;

    let history = history_rows
        .into_iter()
        .map(|(id, instance_id, x)| DumpMessage {
            id,
            instance_id,
            entry: x.0,
        })
        .collect::<Vec<DumpMessage>>();

    Ok(history)
}

#[instrument(level = "trace", skip_all)]
pub async fn insert(pool: &SqlitePool, id: &str, msg: &messages::MessageEntry) -> Result<i32> {
    let json_val =
        serde_json::to_value(msg).context("serializing message entry to history JSON")?;

    let (id,): (i32,) =
        sqlx::query_as("INSERT INTO history (instance_id, json) VALUES (?1, ?2) RETURNING id")
            .bind(id)
            .bind(json_val)
            .fetch_one(pool)
            .await
            .context("inserting message entry into history and returning id")?;

    Ok(id)
}

#[instrument(level = "trace", skip_all)]
pub async fn copy(
    pool: &SqlitePool,
    limit: Option<u16>,
    from: &str,
    to: &str,
    concise: bool,
) -> Result<(u64, Option<messages::MessageId>)> {
    let limit = i64::from(limit.unwrap_or(1000).min(i16::MAX as u16));

    let last_original_id: Option<messages::MessageId> =
        sqlx::query_scalar::<_, messages::MessageId>(AssertSqlSafe(format!(
            "SELECT id FROM ({GET_HISTORY_SELECT} LIMIT ?2) ORDER BY created_at DESC, sorting DESC LIMIT 1"
        )))
        .bind(from)
        .bind(limit)
        .fetch_optional(pool)
        .await
        .context("finding the last copied history record")?;

    let insert_sql = if concise {
        format!(
            "INSERT INTO history (instance_id, json, created_at, sorting) \
             SELECT ?3, json, created_at, sorting FROM ({GET_HISTORY_SELECT}) \
             WHERE NOT ( \
                 json_type(json, '$.SystemEntry.DefinitionsDiff') IS NOT NULL \
                 OR json_type(json, '$.SystemEntry.InstructionDiff') IS NOT NULL \
                 OR json_type(json, '$.SystemEntry.Infusion') IS NOT NULL \
                 OR json_type(json, '$.BotEntry.kind.Reasoning') IS NOT NULL \
             );"
        )
    } else {
        format!(
            "INSERT INTO history (instance_id, json, created_at, sorting) \
             SELECT ?3, json, created_at, sorting FROM ({GET_HISTORY_SELECT});"
        )
    };

    let r = sqlx::query(AssertSqlSafe(insert_sql))
        .bind(from)
        .bind(limit)
        .bind(to)
        .execute(pool)
        .await
        .context("copying history records to another instance_id")?;

    let ra = r.rows_affected();

    trace!("Copied {ra} rows from {from} to {to} (last original id: {last_original_id:?})",);

    Ok((ra, last_original_id))
}
