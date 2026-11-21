use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{
    CommonResources, db,
    providers::embed::{EmbedConfig, embed},
};

#[derive(sqlx::FromRow, Deserialize, Serialize, Debug)]
pub struct Recall {
    pub id: i32,
    pub citation_text: String,
    pub citation_username: String,
    pub text: String,
}

fn down_vec(vec_f64: Vec<f64>) -> Vec<f32> {
    // sadly this is literally the entire purpose of this function
    #[allow(clippy::cast_possible_truncation)]
    vec_f64.into_iter().map(|x| x as f32).collect()
}

#[instrument(level = "trace", skip_all)]
pub async fn save_recall(
    common: &CommonResources,
    embed_config: &EmbedConfig,
    citation: (&str, &str),
    content: &str,
) -> anyhow::Result<()> {
    let embedding = embed(common, embed_config, content).await?;
    db::mem::save_recall_with_embedding(&common.pool, citation, (content, down_vec(embedding)))
        .await
        .map(|_| ())
}

#[instrument(level = "trace", skip_all)]
pub async fn update_recall(
    common: &CommonResources,
    embed_config: &EmbedConfig,
    recall_id: i32,
    citation: Option<(&str, &str)>,
    content: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(content) = content {
        let embedding = embed(common, embed_config, content).await?;
        db::mem::update_recall_with_embedding(
            &common.pool,
            recall_id,
            citation,
            Some((content, down_vec(embedding))),
        )
        .await
    } else {
        db::mem::update_recall_with_embedding(&common.pool, recall_id, citation, None).await
    }
}

#[instrument(level = "trace", skip_all)]
pub async fn search_recalls(
    common: &CommonResources,
    embed_config: &EmbedConfig,
    query: &str,
    limit: u16,
) -> anyhow::Result<Vec<Recall>> {
    let embedding = embed(common, embed_config, query).await?;
    db::mem::search_recalls_with_embedding(&common.pool, down_vec(embedding), limit).await
}

#[instrument(level = "trace", skip_all)]
pub async fn reembed_all(
    common: &CommonResources,
    embed_config: &EmbedConfig,
) -> anyhow::Result<usize> {
    let recalls = db::mem::get_all_recalls(&common.pool).await?;
    let count = recalls.len();

    let tasks = stream::iter(recalls).map(|recall| {
        let common = common.clone();
        async move {
            let embedding = embed(&common, embed_config, &recall.text).await?;
            db::mem::update_recall_with_embedding(
                &common.pool,
                recall.id,
                None,
                Some((&recall.text, down_vec(embedding))),
            )
            .await
        }
    });

    let results: Vec<anyhow::Result<()>> = tasks.buffer_unordered(8).collect().await;

    for res in results {
        res?;
    }

    Ok(count)
}
