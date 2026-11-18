use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use tracing::instrument;

use crate::messages;

#[instrument(level = "trace", skip_all)]
pub fn to_bot_entry(item: &Value) -> Result<Box<messages::BotEntry>> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .context("Output item missing type field")?;

    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let completed = match item.get("status").and_then(Value::as_str) {
        Some("completed") => Some(true),
        Some("incomplete" | "in_progress") => Some(false),
        None => None,
        Some(status) => bail!("Unknown output item status: {status}"),
    };

    let kind = match item_type {
        "message" => {
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .context("Message item missing role")?;

            if role != "assistant" {
                bail!("Unexpected message role in output: {role}");
            }

            let mut content_parts = Vec::new();
            if let Some(content_array) = item.get("content").and_then(Value::as_array) {
                for part in content_array {
                    ensure!(
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("output_text")
                        ),
                        "content array part must be output_text"
                    );

                    let text = part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    let annotations = part
                        .get("annotations")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();

                    let logprobs = part
                        .get("logprobs")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();

                    content_parts.push(messages::BotMessageContent::Text(
                        messages::BotTextContent {
                            text,
                            annotations,
                            logprobs,
                        },
                    ));
                }
            }

            let refusal = item
                .get("refusal")
                .and_then(Value::as_str)
                .map(str::to_string);

            messages::BotEntryKind::BotMessage(messages::BotMessage {
                content: content_parts,
                refusal,
            })
        }

        "reasoning" => {
            let mut summary = Vec::new();

            if let Some(summary_v) = item.get("summary") {
                let v_summary = summary_v.as_array().context("Summary not an array")?;

                for part in v_summary {
                    ensure!(
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("summary_text")
                        ),
                        "reasoning summary array part must be summary_text"
                    );
                    let text = part
                        .get("text")
                        .context("Text missing from summary text")?
                        .as_str()
                        .context("Text field in summary text is not string")?;
                    summary.push(text.to_string());
                }
            }

            let mut content = Vec::new();

            if let Some(content_v) = item.get("content") {
                let v_content = content_v.as_array().context("Content not an array")?;

                for part in v_content {
                    ensure!(
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("reasoning_text")
                        ),
                        "reasoning content array part must be reasoning_text or summary_text"
                    );
                    let text = part
                        .get("text")
                        .context("Text missing from content reasoning text")?
                        .as_str()
                        .context("Text field in content reasoning text is not string")?;
                    content.push(text.to_string());
                }
            }

            let signature = item
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string);

            let encrypted_content = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string);

            messages::BotEntryKind::Reasoning(messages::Reasoning {
                signature,
                summary,
                content,
                encrypted_content,
            })
        }

        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .context("function_call missing call_id")?;

            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("function_call missing name")?;

            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .context("function_call missing arguments")?;

            let signature = item
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string);

            messages::BotEntryKind::ToolCall(messages::ToolCall {
                tool_name: name.to_string(),
                remote_call_id: call_id.to_string(),
                local_call_id: nanoid::nanoid!(),
                tool_input: arguments.to_string(),
                signature,
            })
        }

        "web_search_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("web_search_call missing call_id")?;
            let query = item
                .get("query")
                .and_then(Value::as_str)
                .context("web_search_call missing query")?;
            let results = item
                .get("results")
                .and_then(Value::as_array)
                .map_or_default(|arr| {
                    arr.iter()
                        .filter_map(|r| {
                            Some(messages::WebSearchResult {
                                url: r.get("url")?.as_str()?.to_string(),
                                title: r.get("title")?.as_str()?.to_string(),
                                snippet: r.get("snippet")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                });

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::WebSearch(
                messages::WebSearchCall {
                    call_id: call_id.to_string(),
                    query: query.to_string(),
                    results,
                },
            ))
        }

        "code_interpreter_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("code_interpreter_call missing call_id")?;
            let code = item
                .get("code")
                .and_then(Value::as_str)
                .context("code_interpreter_call missing code")?;
            let output = item
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string);

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::CodeInterpreter(
                messages::CodeInterpreterCall {
                    call_id: call_id.to_string(),
                    code: code.to_string(),
                    output,
                },
            ))
        }

        "computer_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("computer_call missing call_id")?;
            let action = item.get("action").context("computer_call missing action")?;
            let output = item.get("output").cloned();

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::Computer(
                messages::ComputerCall {
                    call_id: call_id.to_string(),
                    action: action.clone(),
                    output,
                },
            ))
        }

        "image_generation_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("image_generation_call missing call_id")?;
            let prompt = item
                .get("prompt")
                .and_then(Value::as_str)
                .context("image_generation_call missing prompt")?;
            let images = item
                .get("images")
                .and_then(Value::as_array)
                .map_or_default(|arr| {
                    arr.iter()
                        .filter_map(|img| {
                            Some(messages::GeneratedImage {
                                url: img.get("url")?.as_str()?.to_string(),
                                revised_prompt: img
                                    .get("revised_prompt")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            })
                        })
                        .collect()
                });

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::ImageGeneration(
                messages::ImageGenerationCall {
                    call_id: call_id.to_string(),
                    prompt: prompt.to_string(),
                    images,
                },
            ))
        }

        "local_shell_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("local_shell_call missing call_id")?;
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .context("local_shell_call missing command")?;
            let output = item
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string);

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::LocalShell(
                messages::LocalShellCall {
                    call_id: call_id.to_string(),
                    command: command.to_string(),
                    output,
                },
            ))
        }

        "mcp_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("mcp_call missing call_id")?;
            let server = item
                .get("server")
                .and_then(Value::as_str)
                .context("mcp_call missing server")?;
            let method = item
                .get("method")
                .and_then(Value::as_str)
                .context("mcp_call missing method")?;
            let arguments = item
                .get("arguments")
                .context("mcp_call missing arguments")?;
            let result = item.get("result").cloned();

            messages::BotEntryKind::RemoteToolCall(messages::RemoteToolCall::Mcp(
                messages::McpCall {
                    call_id: call_id.to_string(),
                    server: server.to_string(),
                    method: method.to_string(),
                    arguments: arguments.clone(),
                    result,
                },
            ))
        }

        other => bail!("Unknown output item type: {other}"),
    };

    Ok(Box::new(messages::BotEntry {
        id,
        completed,
        kind,
    }))
}
