use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{CommonResources, config::snapshot_config};

/// Configures vector embeddings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// API key for authenticating to endpoint
    pub api_key: String,
    /// What embedding model to request
    pub model: String,
    /// Base URL for embedding endpoint
    pub base_url: String,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f64>,
}

#[instrument(level = "debug", skip(common, content))]
pub async fn embed(common: &CommonResources, content: &str) -> Result<Vec<f64>> {
    let config = snapshot_config(&common.config).await;

    let Some(embed_config) = &config.providers.embed else {
        bail!("Embedding disabled");
    };

    let url = format!(
        "{}/v1/embeddings",
        embed_config.base_url.trim_end_matches('/')
    );

    let response = common
        .http
        .post(&url)
        .bearer_auth(&embed_config.api_key)
        .json(&serde_json::json!({
            "model": embed_config.model,
            "input": content,
        }))
        .send()
        .await
        .context("Sending embedding request")?;

    let status = response.status();

    let body: EmbeddingsResponse = response
        .json()
        .await
        .with_context(|| format!("Decoding embeddings response (status {status})"))?;

    body.data
        .into_iter()
        .next()
        .map(|data| data.embedding)
        .context("Embeddings response contained no data")
}
