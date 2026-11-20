use std::borrow::Cow;
use std::path::PathBuf;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_inline_default::serde_inline_default;
use serde_json::{Value, json};
use std::fmt::Write;
use thiserror::Error;
use tokio::fs;
use tracing::{error, instrument, trace};

use crate::config::ConfigSnapshot;
use crate::messages;

mod from_openai;
pub mod openai;
mod to_openai;
mod validate;

#[cfg(test)]
mod tests;

use openai::StreamMessage;

/// Constraints for LLM generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConstraints {
    /// How many tokens are allowed to be generated in a single request
    pub max_gen: u32,
    /// How many thinking tokens are allowed to be generated before being cut off
    pub max_think: u32,
}

fn default_supported_file_extensions() -> Vec<String> {
    vec![
        String::from("jpg"),
        String::from("jpeg"),
        String::from("png"),
        String::from("gif"),
        String::from("svg"),
    ]
}

/// Configuration for LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// API key to authenticate to LLM endpoint
    pub api_key: String,
    /// Which model to request
    pub model: String,
    /// Base URL of LLM endpoint
    pub base_url: String,
    /// Maximum context this models supports (or to allow)
    pub max_ctx: u32,
    /// Default constraints to use when no others are supplied
    pub default_constraints: LlmConstraints,
    /// Perform additional validation that history is perfectly reconstructed
    #[serde(default)]
    pub validate: bool,
    /// Which file extensions are supported as attachments
    #[serde(default = "default_supported_file_extensions")]
    pub supported_file_extensions: Vec<String>,
    /// If this endpoint requires tool results to follow directly after calls
    #[serde(default)]
    pub results_must_follow_calls: bool,

    /// If you have hacked your chat template to show `shown_tools` rather than tools,
    /// used to make tool diffs work instantly without prompt cache invalidation :)
    #[serde(default)]
    pub special_shown_definitions: bool,

    /// Where to optionally write a dump of openai history for debugging
    #[serde(default)]
    pub openai_history_dump: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl LlmTool {
    pub fn to_openai_json(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters
        })
    }
}

pub enum LlmOutput {
    Entry(bool, Box<messages::BotEntry>, Option<openai::Usage>),
    End(openai::Usage),
}

#[instrument(level = "trace", skip_all, fields(history.len = %history.len()))]
pub async fn input_tokens(
    config: &ConfigSnapshot,
    client: &reqwest::Client,
    base_prompt: &messages::BasePrompt,
    history: &[messages::Message],
) -> Result<u64> {
    let llm_config = &config.providers.llm;

    let body = request_body(
        &config.providers.llm,
        None,
        base_prompt,
        history,
        false,
        None,
    )
    .await?;

    let r = openai::input_tokens(client, &llm_config.base_url, &llm_config.api_key, body)
        .await
        .context("Failed to get input tokens")?;

    Ok(r)
}

#[derive(Debug, Error)]
pub enum CallError {
    #[error("OpenAI error: {0}")]
    OpenAI(openai::CallError),
    #[error("To-OpenAI error: {0}")]
    ToOpenAI(anyhow::Error),
    #[error("Validation error: {0}")]
    Validate(anyhow::Error),
}

#[instrument(level = "trace", skip_all, fields(history.len = %history.len()))]
pub async fn stream_llm_messages(
    config: &ConfigSnapshot,
    client: &reqwest::Client,
    constraints_override: Option<&LlmConstraints>,
    base_prompt: &messages::BasePrompt,
    history: &[messages::Message],
    id_slot: Option<u8>,
) -> std::result::Result<impl futures::Stream<Item = Result<LlmOutput>>, CallError> {
    let llm_config = &config.providers.llm;

    if llm_config.validate {
        validate::validate_history_round_trip(history)
            .await
            .map_err(CallError::Validate)?;
    }

    let body = request_body(
        &config.providers.llm,
        constraints_override,
        base_prompt,
        history,
        true,
        id_slot,
    )
    .await?;

    let mut wire = Box::pin(
        openai::stream(client, &llm_config.base_url, &llm_config.api_key, body)
            .await
            .map_err(CallError::OpenAI)?,
    );

    let translated = async_stream::try_stream! {
        while let Some(message) = wire.next().await {
            match message? {
                StreamMessage::End(usage) => {
                    yield LlmOutput::End(usage);
                },

                StreamMessage::Partial(message, m_usage) => {
                    let entry = from_openai::to_bot_entry(&message).context("Translating partial OpenAI message")?;
                    yield LlmOutput::Entry(false, entry, m_usage);
                }

                StreamMessage::Complete(message, m_usage) => {
                    let entry = from_openai::to_bot_entry(&message).context("Translating complete OpenAI message")?;

                    if llm_config.validate {
                        validate::validate_output_round_trip(&entry, &message)
                            .await
                            .context("Validating OpenAI output round trip")?;
                    }

                    trace!("final: {:?}", entry);
                    yield LlmOutput::Entry(true, entry, m_usage);
                }
            }
        }
    };

    Ok(translated)
}

#[instrument(level = "trace", skip_all)]
async fn request_body(
    config: &LlmConfig,
    constraints_override: Option<&LlmConstraints>,
    base_prompt: &messages::BasePrompt,
    history: &[messages::Message],
    stream: bool,
    id_slot: Option<u8>,
) -> std::result::Result<Value, CallError> {
    let openai_history = to_openai::to_openai_messages(history)
        .await
        .map_err(CallError::ToOpenAI)?;

    if let Some(p) = config.openai_history_dump.clone() {
        let openai_history = openai_history.clone();
        tokio::spawn(async move {
            let r = async || {
                let openai_history_json =
                    serde_json::to_string(&openai_history).context("Serializing openai history")?;
                fs::write(p, openai_history_json)
                    .await
                    .context("Writing openai history dump")?;

                anyhow::Ok(())
            };

            if let Err(e) = r().await {
                error!("Failed to write openai history dump: {e}");
            }
        });
    };

    let constraints = constraints_override.unwrap_or(&config.default_constraints);
    let thinking = constraints.max_think > 0;

    let mut kwargs = json!({
        "enable_thinking": thinking,
        "reasoning_control": thinking,
        "reasoning_effort": "low",
        "thinking_budget_tokens": constraints.max_think,
        "reasoning_format": "auto",
    });

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(config.model));
    body.insert("input".to_string(), json!(openai_history));
    body.insert("max_tokens".to_string(), json!(config.max_ctx));
    body.insert(
        "max_completion_tokens".to_string(),
        json!(constraints.max_gen),
    );
    body.insert("timings_per_token".to_string(), json!(true));
    body.insert("backend_sampling".to_string(), json!(false));
    body.insert(
        "reasoning".to_string(),
        json!({
            "effort": "low",
        }),
    );
    body.insert("max_output_tokens".to_string(), json!(constraints.max_gen));

    if stream {
        body.insert("stream".to_string(), json!(true));
    }

    let mut prompt_instructions = Cow::Borrowed(&base_prompt.instructions);

    if !base_prompt.shown_definitions.is_empty() && config.special_shown_definitions {
        kwargs["shown_tools"] = json!(
            base_prompt
                .shown_definitions
                .values()
                .flat_map(|d| &d.tools)
                .map(LlmTool::to_openai_json)
                .collect::<Vec<_>>()
        );

        for (name, definition) in &base_prompt.shown_definitions {
            if let Some(definition_instructions) = &definition.instructions {
                let prompt_instructions = prompt_instructions.to_mut();

                let tool_name_list_str = definition
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");

                let _ = write!(
                    prompt_instructions,
                    "\n\nInstructions for `{name}` tools ({tool_name_list_str}):\n{definition_instructions}",
                );
            }
        }
    }

    body.insert("instructions".to_string(), json!(prompt_instructions));
    body.insert("chat_template_kwargs".to_string(), kwargs);

    let real_definitions = if config.special_shown_definitions {
        &base_prompt.real_definitions
    } else {
        &base_prompt.shown_definitions
    };

    if !base_prompt.real_definitions.is_empty() {
        body.insert(
            "tools".to_string(),
            json!(
                real_definitions
                    .values()
                    .flat_map(|d| &d.tools)
                    .map(LlmTool::to_openai_json)
                    .collect::<Vec<_>>()
            ),
        );
    }

    if let Some(slot) = id_slot {
        body.insert("id_slot".to_string(), json!(slot));
    }

    Ok(Value::Object(body))
}
