pub mod mcp;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use tracing::trace;

use crate::{
    agent::AgentConfig, config::ConfigSnapshot, messages, providers::llm::LlmTool,
    utils::sandbox::SandboxConfig,
};

use mcp::McpConfig;

#[async_trait]
pub trait BotTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self, config_snapshot: &ConfigSnapshot, sub_config: &AgentConfig) -> String;
    async fn parameters(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
    ) -> serde_json::Value;
    async fn call_json(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)>;

    async fn definition(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
    ) -> LlmTool {
        LlmTool {
            name: self.name().to_string(),
            description: self.description(config_snapshot, sub_config),
            parameters: self.parameters(config_snapshot, sub_config).await,
        }
    }
}

/// Configures agent tooling
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolingConfig {
    /// Configuration for MCP servers
    pub mcp: McpConfig,
}

pub struct BotToolSet {
    with_mcp: bool,
    tools: Vec<Box<dyn BotTool>>,
}

impl BotToolSet {
    pub fn create(with_mcp: bool, tools: Vec<Box<dyn BotTool>>) -> Self {
        Self { with_mcp, tools }
    }
}

impl BotToolSet {
    pub async fn get_definitions(
        &self,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
    ) -> Result<BTreeMap<String, messages::ToolingDefinition>> {
        trace!("Getting tool definitions from bot tools");

        let mut self_tools = Vec::new();

        for tool in &self.tools {
            self_tools.push(tool.definition(config_snapshot, sub_config).await);
        }

        let mut definitions = BTreeMap::new();

        definitions.insert(
            "self".to_string(),
            messages::ToolingDefinition {
                instructions: None,
                tools: self_tools,
            },
        );

        if self.with_mcp {
            trace!("Getting tool definitions from mcp");

            let mcp_definitions = mcp::definitions(
                config_snapshot,
                &sub_config.tooling.mcp,
                &sub_config.sandbox,
            )
            .await
            .context("getting mcp tools")?;

            for (server_name, definition) in mcp_definitions {
                definitions.insert(server_name, definition);
            }
        }

        Ok(definitions)
    }

    pub async fn call(
        &self,
        http: &reqwest::Client,
        config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)> {
        if let Some(tool) = self.tools.iter().find(|tool| tool.name() == tool_name) {
            Ok(tool
                .call_json(config_snapshot, sub_config, args)
                .await
                .with_context(|| format!("Calling tool '{tool_name}'"))?)
        } else if self.with_mcp {
            let mcp_r = mcp::call_tool(
                http,
                config_snapshot,
                &sub_config.tooling.mcp,
                &sub_config.sandbox,
                tool_name,
                &args,
            )
            .await
            .with_context(|| format!("Calling tool '{tool_name}'"))?;
            Ok(mcp_r)
        } else {
            bail!("No such tool, and MCP disabled");
        }
    }
}
