use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

use crate::CommonResources;
use crate::agent::AgentConfig;
use crate::config::ConfigSnapshot;
use crate::db;
use crate::mem;
use crate::utils::tooling;

#[derive(Deserialize)]
pub struct SearchMemArgs {
    query: String,
}

pub struct SearchMemory(pub CommonResources);

#[derive(Serialize, Deserialize)]
pub struct SearchMemoryResult {
    text: String,
    id: i32,
}

#[async_trait]
impl tooling::BotTool for SearchMemory {
    fn name(&self) -> &'static str {
        "search_memory"
    }

    fn description(&self, _config_snapshot: &ConfigSnapshot, _sub_config: &AgentConfig) -> String {
        "Searches information you have SAVED using a SIMPLE search query. Can only find information that has already been saved (NOT chat logs or messages, IF YOU DO NOT CONSTANTLY SAVE, THIS WILL FAIL YOU). Identical queries ALWAYS return the SAME RESULTS. DO NOT REPEAT SEARCHES. Search BEFORE RUNNING COMMANDS, BEFORE RESPONDING, and BEFORE DOING ANYTHING".to_string()
    }

    async fn parameters(
        &self,
        _config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
    ) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "SIMPLE AND VERY SHORT SEARCH QUERY. DO NOT chain queries together, keep it extremely simple and short (uses a tiny vector search model). Even mildly complex queries will find BAD RESULTS. GOOD EXAMPLE: \"what color is person1's hat\""
                }
            },
            "required": ["query"]
        })
    }
    async fn call_json(
        &self,
        config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)> {
        let args: SearchMemArgs =
            serde_json::from_value(args).context("parsing search_memory tool arguments")?;

        let result: Vec<SearchMemoryResult> = mem::search_recalls(
            &self.0,
            config_snapshot
                .providers
                .embed
                .as_ref()
                .context("Embedding is disabled")?,
            &args.query,
            config_snapshot.instance.memory.search_limit,
            false,
        )
        .await
        .context("searching memories")?
        .into_iter()
        .map(|x| SearchMemoryResult {
            text: x.text,
            id: x.id,
        })
        .collect();
        Ok((
            serde_json::to_string(&result).context("serializing search_memory result")?,
            vec![],
        ))
    }
}

#[derive(Deserialize)]
pub struct SaveMemArgs {
    content: String,
    citation_username: String,
    citation_text: String,
    replace_existing_id: Option<i32>,
}

pub struct SaveMemory(pub CommonResources);

#[async_trait]
impl tooling::BotTool for SaveMemory {
    fn name(&self) -> &'static str {
        "save_memory"
    }

    fn description(&self, _config_snapshot: &ConfigSnapshot, _sub_config: &AgentConfig) -> String {
        "Saves information you have learned, which is MANDATORY to use whenever applicable to avoid forgetting. YOU MUST NOT USE THIS WITHOUT SEARCHING MEMORIES FIRST. EVEN IN THE MOST UNLIKELY CIRCUMSTANCES, YOU MUST ALWAYS USE MEMORY SEARCH AND NEVER EVER EVER USE SAVE WITHOUT SEARCHING MEMORY FIRST"
            .to_string()
    }

    async fn parameters(
        &self,
        _config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
    ) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The information, which must make sense on a Wiki without seeing context or chat logs, abiding by the saving rules AND BE SOMETHING YOU PREVIOUSLY SEARCHED YOUR MEMORY FOR BEFORE SAVING"
                },
                "citation_username": {
                    "type": "string",
                    "description": "Chat message to cite as the source of this information"
                },
                "citation_text": {
                    "type": "string",
                    "description": "Chat message to cite as the source of this information"
                },
                "replace_existing_id": {
                    "type": "number",
                    "description": "Specifies a save this is an UPDATE of. ONLY SPECIFICY IF this information SUPERCEDES AND REPLACES THE ORIGINAL INFORMATION, providing MORE UPDATED INFORMATION"
                }
            },
            "required": ["content", "citation_username", "citation_text"]
        })
    }
    async fn call_json(
        &self,
        config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)> {
        let args: SaveMemArgs =
            serde_json::from_value(args).context("parsing save_memory tool arguments")?;

        if let Some(existing_id) = args.replace_existing_id {
            mem::update_recall(
                &self.0,
                config_snapshot
                    .providers
                    .embed
                    .as_ref()
                    .context("Embedding is disabled")?,
                existing_id,
                Some((&args.citation_username, &args.citation_text)),
                Some(&args.content),
            )
            .await
            .context("updating memory")?;
        } else {
            mem::save_recall(
                &self.0,
                config_snapshot
                    .providers
                    .embed
                    .as_ref()
                    .context("Embedding is disabled")?,
                (&args.citation_username, &args.citation_text),
                &args.content,
            )
            .await
            .context("saving memory")?;
        }

        Ok(("Memory saved".to_string(), vec![]))
    }
}

#[derive(Deserialize)]
pub struct DeleteMemArgs {
    ids: Vec<i32>,
}

pub struct DeleteMemory(pub CommonResources);

#[async_trait]
impl tooling::BotTool for DeleteMemory {
    fn name(&self) -> &'static str {
        "delete_memories"
    }

    fn description(&self, _config_snapshot: &ConfigSnapshot, _sub_config: &AgentConfig) -> String {
        "Erases saved information. Use when you are CERTAIN it is a duplicate, or 100% sure it violates a rule (if simple INACCURATE, update the save instead)".to_string()
    }

    async fn parameters(
        &self,
        _config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
    ) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "ids": {
                    "description": "Save IDs to clean (if removing duplicates DO NOT REMOVE ALL)",
                    "type": "array",
                    "items": {
                        "type": "number",
                        "description": "ID of the save to delete"
                    }
                }
            },
            "required": ["ids"]
        })
    }
    async fn call_json(
        &self,
        _config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)> {
        let args: DeleteMemArgs =
            serde_json::from_value(args).context("parsing delete_memories tool arguments")?;
        for id in args.ids {
            db::mem::delete_recall(&self.0.pool, id)
                .await
                .with_context(|| format!("deleting memory {id}"))?;
        }

        Ok(("Memories deleted".to_string(), vec![]))
    }
}
