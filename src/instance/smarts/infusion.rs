use anyhow::{Context as _, Result};
use indoc::indoc;
use rand::rng;
use rand::seq::IndexedRandom as _;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashSet;
use tracing::instrument;

use crate::{CommonResources, db, mem, messages};

use formatx::formatx;

/// Configures automatic memory infusions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InfusionConfig {
    /// How many memories to search for (sorted by relevance)
    pub search_size: u16,
    /// How many memories to reduce this to at random
    pub chunk_size: u16,
    /// How many total memories to deliver in the infusion
    pub size_limit: u16,
    /// How long ago should a memory have been read before it is allowed to be infused
    pub repeat_after: u16,

    /// Sets of query strings to be used
    pub queries: Vec<Vec<String>>,
    /// Messages to be used by the infusion system
    pub messages: InfusionMessages,
}

impl Default for InfusionConfig {
    fn default() -> Self {
        Self {
            search_size: 10,
            chunk_size: 5,
            size_limit: 4,
            repeat_after: 200,
            queries: vec![
                vec![
                    "facts about {author_name}".to_string(),
                    "{author_name} preferences".to_string(),
                    "what to say to {author_name}".to_string(),
                ],
                vec![
                    "facts about \"{message_text}\"".to_string(),
                    "{author_name}: {message_text}".to_string(),
                    "what does \"{message_text}\" mean".to_string(),
                    "response to \"{message_text}\"".to_string(),
                ],
                vec!["{author_name} saying \"{message_text}\"".to_string()],
            ],
            messages: InfusionMessages::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InfusionMessages {
    /// Template used to deliver infusions
    pub infusion: String,
}

impl Default for InfusionMessages {
    fn default() -> Self {
        Self {
            infusion: String::from(indoc! {"
                Automatic infusion of random information:
                {infusion_str}
            "}),
        }
    }
}

fn select_memory_queries(
    queries: &[Vec<String>],
    author_name: &str,
    message_text: &str,
) -> Result<Vec<String>> {
    let mut selected_queries = Vec::new();

    for group in queries {
        if group.is_empty() {
            continue;
        }
        let template = group
            .choose(&mut rng())
            .context("choosing infusion query template from group")?;

        let query = formatx!(
            template,
            author_name = author_name,
            message_text = message_text
        )
        .context("formatting infusion query template")?;

        selected_queries.push(query);
    }

    Ok(selected_queries)
}

fn clumpify_text(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect()
}

#[instrument(level = "trace", skip_all)]
async fn fetch_recent_history_clump(
    instance_id: &str,
    pool: &SqlitePool,
    limit: u16,
) -> Result<String> {
    let history = db::history::get_history(pool, instance_id, Some(limit)).await?;

    let history_json = serde_json::to_string(&history)
        .context("serializing recent history for memory infusion")?;

    Ok(clumpify_text(&history_json))
}

#[instrument(level = "trace", skip_all)]
async fn collect_infusion_memories(
    common: &CommonResources,
    queries: Vec<String>,
    history_clump: &str,
    search_size: u16,
    chunk_size: u16,
) -> Result<HashSet<serde_json::Value>> {
    let mut memories = HashSet::new();

    // Search for memories matching each query
    for query in queries {
        let results = mem::search_recalls(common, &query, search_size).await?;

        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| !history_clump.contains(&clumpify_text(&r.text)))
            .collect();

        // Randomly select up to chunk_size items
        for memory_item in filtered.iter().take(chunk_size as usize) {
            memories.insert(json!({ "text": memory_item.text, "id": memory_item.id }));
        }
    }

    // Add random memories as fallback
    if memories.len() < chunk_size as usize {
        let all_memories = db::mem::get_all_recalls(&common.pool).await?;

        let filtered: Vec<_> = all_memories
            .into_iter()
            .filter(|r| !history_clump.contains(&clumpify_text(&r.text)))
            .collect();

        for memory_item in filtered.sample(
            &mut rng(),
            (chunk_size as usize).saturating_sub(memories.len()),
        ) {
            memories.insert(json!({ "text": memory_item.text, "id": memory_item.id }));
        }
    }

    Ok(memories)
}

#[instrument(level = "trace", skip_all)]
pub async fn create_infusion_message(
    common: &CommonResources,
    instance_id: &str,
    platform_msg: &messages::PlatformMessage,
    infusion_config: &InfusionConfig,
) -> Result<Option<messages::MessageEntry>> {
    let queries = select_memory_queries(
        &infusion_config.queries,
        &platform_msg.author_name,
        &platform_msg.text,
    )?;

    let history_clump =
        fetch_recent_history_clump(instance_id, &common.pool, infusion_config.repeat_after).await?;

    let memories = collect_infusion_memories(
        common,
        queries,
        &history_clump,
        infusion_config.search_size,
        infusion_config.chunk_size,
    )
    .await?;

    if memories.is_empty() {
        return Ok(None);
    }

    let selected_memories: Vec<_> = memories
        .into_iter()
        .take(infusion_config.size_limit as usize)
        .collect();

    let infusion_str = serde_json::to_string(&selected_memories)
        .context("serializing selected memories for infusion")?;

    let rendered_infusion_template = {
        let infusion_msg = &infusion_config.messages.infusion;
        formatx!(infusion_msg, infusion_str = infusion_str)
            .context("formatting infusion template invalid")?
    };

    Ok(Some(messages::MessageEntry::SystemEntry(
        messages::SystemEntry::Infusion(rendered_infusion_template),
    )))
}
