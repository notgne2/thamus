use anyhow::{Context as _, Result, ensure};
use tracing::instrument;
use zerocopy::IntoBytes as _;

use crate::mem::Recall;

/// Must match the dimension the embedding provider produces.
pub const EMBEDDING_DIMENSION: usize = 1024;

fn check_dimension(embedding: &[f32]) -> Result<()> {
    ensure!(
        embedding.len() == EMBEDDING_DIMENSION,
        "expected {EMBEDDING_DIMENSION} embedding dimensions, got {}",
        embedding.len()
    );
    Ok(())
}

#[instrument(level = "trace", skip_all)]
pub async fn delete_recall(pool: &sqlx::SqlitePool, recall_id: i32) -> Result<()> {
    sqlx::query("UPDATE recalls SET hidden = 1 WHERE id = ?1")
        .bind(recall_id)
        .execute(pool)
        .await
        .context("hiding recall")?;

    Ok(())
}

#[instrument(level = "trace", skip_all)]
pub async fn save_recall_with_embedding(
    pool: &sqlx::SqlitePool,

    citation: (&str, &str),

    content: (&str, Vec<f32>),
) -> Result<i32> {
    check_dimension(&content.1)?;

    let next_id: i32 = sqlx::query_scalar(
        "INSERT INTO recalls (embedding, hidden, citation_text, citation_username, text)
             VALUES (?1, 0, ?2, ?3, ?4)
             RETURNING id",
    )
    .bind(content.1.as_bytes())
    .bind(citation.1)
    .bind(citation.0)
    .bind(content.0)
    .fetch_one(pool)
    .await
    .context("inserting recall")?;

    Ok(next_id)
}

#[instrument(level = "trace", skip_all)]
pub async fn update_recall_with_embedding(
    pool: &sqlx::SqlitePool,

    recall_id: i32,

    citation: Option<(&str, &str)>,

    content: Option<(&str, Vec<f32>)>,
) -> Result<()> {
    if let Some((text, embedding)) = content.as_ref() {
        check_dimension(embedding)?;

        sqlx::query(
            "UPDATE recalls
                SET embedding = ?2, text = ?3,
                    citation_text = COALESCE(?4, citation_text),
                    citation_username = COALESCE(?5, citation_username)
              WHERE id = ?1",
        )
        .bind(recall_id)
        .bind(embedding.as_bytes())
        .bind(text)
        .bind(citation.map(|c| c.1))
        .bind(citation.map(|c| c.0))
        .execute(pool)
        .await
        .context("updating recall")?;
    } else {
        sqlx::query(
            "UPDATE recalls
                SET citation_text = COALESCE(?2, citation_text),
                    citation_username = COALESCE(?3, citation_username)
              WHERE id = ?1",
        )
        .bind(recall_id)
        .bind(citation.map(|c| c.1))
        .bind(citation.map(|c| c.0))
        .execute(pool)
        .await
        .context("updating recall")?;
    }

    Ok(())
}

type RecallRow = (i32, String, String, String);

#[instrument(level = "trace", skip_all)]
pub async fn search_recalls_with_embedding(
    pool: &sqlx::SqlitePool,

    query_embedding: Vec<f32>,
    limit: u16,
) -> Result<Vec<Recall>> {
    check_dimension(&query_embedding)?;

    let k = i64::from(limit.clamp(1, 1000));

    let rows: Vec<RecallRow> = sqlx::query_as(
        "
            SELECT id, citation_text, citation_username, text
              FROM recalls
             WHERE embedding MATCH ?1
               AND k = ?2
               AND hidden != 1
             ORDER BY distance
            ",
    )
    .bind(query_embedding.as_bytes())
    .bind(k)
    .fetch_all(pool)
    .await
    .context("searching recalls by embedding similarity")?;

    Ok(rows
        .into_iter()
        .map(|(id, citation_text, citation_username, text)| Recall {
            id,
            citation_text,
            citation_username,
            text,
        })
        .collect())
}

#[instrument(level = "trace", skip_all)]
pub async fn get_all_recalls(pool: &sqlx::SqlitePool) -> Result<Vec<Recall>> {
    let rows: Vec<RecallRow> = sqlx::query_as(
        "SELECT id, citation_text, citation_username, text FROM recalls WHERE COALESCE(hidden, 0) = 0",
    )
    .fetch_all(pool)
    .await
    .context("fetching all recalls")?;

    Ok(rows
        .into_iter()
        .map(|(id, citation_text, citation_username, text)| Recall {
            id,
            citation_text,
            citation_username,
            text,
        })
        .collect())
}
