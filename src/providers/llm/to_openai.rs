use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::AsyncReadExt as _;
use tracing::instrument;

use crate::messages;
use crate::utils::formats::{ext_format_supported, mime_for_ext, type_for_mime};

#[instrument(level = "trace", skip_all)]
async fn attachment_to_json(attachment: &Path) -> Result<Option<Value>> {
    let ext = match attachment.extension() {
        Some(e) => e
            .to_str()
            .context("Attachment extension contains non-stringy bytes")?,
        None => "idk",
    };

    if !ext_format_supported(ext) {
        return Ok(None);
    }

    let mime = mime_for_ext(ext, "");
    let t = type_for_mime(&mime);

    let mut file = fs::File::open(attachment).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .await
        .context("Reading attachment file")?;
    let encoded = STANDARD.encode(&bytes);

    let item = match t {
        "image" => {
            json!({
                "type": "input_image",
                "image_url": format!("data:{mime};base64,{encoded}")
            })
        }
        "audio" => json!({
            "type": "input_audio",
            "input_audio": { "data": encoded, "format": ext }
        }),
        _ => json!({
            "type": "input_file",
            "data": format!("data:{mime};base64,{encoded}"),
            "format": ext
        }),
    };

    Ok(Some(item))
}

#[instrument(level = "trace", skip_all)]
async fn tool_result_content_to_output(
    content: &str,
    attachments: &[PathBuf],
) -> Result<Vec<serde_json::Value>> {
    let mut output = Vec::new();

    output.push(json!({
        "type": "input_text",
        "text": content,
    }));

    for attachment in attachments {
        if let Some(a) = attachment_to_json(attachment).await? {
            output.push(a);
        }
    }

    Ok(output)
}

#[instrument(level = "trace", skip_all)]
pub async fn to_openai_message(
    message: &messages::Message,
) -> Result<serde_json::Map<String, Value>> {
    let m = match &message.entry {
        messages::MessageEntry::SystemEntry(system_entry) => match system_entry {
            messages::SystemEntry::ToolResult(tool_result) => {
                let output =
                    tool_result_content_to_output(&tool_result.result, &tool_result.attachments)
                        .await
                        .context("Failed to serialize tool result content")?;

                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), json!("function_call_output"));
                obj.insert("call_id".to_string(), json!(tool_result.remote_call_id));
                obj.insert("name".to_string(), json!(tool_result.tool_name));
                obj.insert("output".to_string(), json!(output));
                obj
            }

            messages::SystemEntry::Compaction(x)
            | messages::SystemEntry::InstructionDiff(x)
            | messages::SystemEntry::DefinitionsDiff(x)
            | messages::SystemEntry::Misc(x)
            | messages::SystemEntry::Infusion(x) => {
                let text = format!("system: {x}");

                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), json!("message"));
                obj.insert("role".to_string(), json!("user"));
                obj.insert(
                    "content".to_string(),
                    json!([{"type": "input_text", "text": text}]),
                );
                obj
            }
        },

        messages::MessageEntry::UserMessage(user_message) => {
            let mut content_items = Vec::new();

            for part in &user_message.content {
                match part {
                    messages::MessageContent::Text(t) => {
                        let text = if user_message.username.is_empty() {
                            t.clone()
                        } else {
                            format!("<{}>: {}", user_message.username, t)
                        };
                        content_items.push(json!({
                            "type": "input_text",
                            "text": text
                        }));
                    }
                    messages::MessageContent::Attachment(attachment) => {
                        if let Some(att_json) = attachment_to_json(attachment).await? {
                            content_items.push(att_json);
                        }
                    }
                }
            }

            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), json!("message"));
            obj.insert("role".to_string(), json!("user"));
            obj.insert("content".to_string(), json!(content_items));
            obj
        }

        messages::MessageEntry::Error(err) => {
            let err_s = format!("[Error: {err}]");

            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), json!("message"));
            obj.insert("role".to_string(), json!("assistant"));
            obj.insert(
                "content".to_string(),
                json!([{
                    "type": "output_text",
                    "text": err_s,
                    "annotations": [],
                    "logprobs": [],
                }]),
            );
            obj
        }

        messages::MessageEntry::BotEntry(bot_entry) => {
            let mut item: serde_json::Map<String, Value> = match &bot_entry.kind {
                messages::BotEntryKind::BotMessage(bot_message) => {
                    let mut parts = Vec::new();
                    for part in &bot_message.content {
                        if let messages::BotMessageContent::Text(t) = part {
                            let mut obj = serde_json::Map::new();
                            obj.insert("type".to_string(), json!("output_text"));
                            obj.insert("text".to_string(), json!(t.text));
                            obj.insert("annotations".to_string(), json!(t.annotations));
                            obj.insert("logprobs".to_string(), json!(t.logprobs));

                            parts.push(Value::Object(obj));
                        }
                        // Attachment is local-only, not sent to the LLM
                    }

                    let mut item = serde_json::Map::new();
                    item.insert("type".to_string(), json!("message"));
                    item.insert("role".to_string(), json!("assistant"));
                    item.insert("content".to_string(), json!(parts));
                    if let Some(refusal) = &bot_message.refusal {
                        item.insert("refusal".to_string(), json!(refusal));
                    }

                    item
                }

                messages::BotEntryKind::ToolCall(tool_call) => tool_call_to_json(tool_call)?,

                messages::BotEntryKind::RemoteToolCall(remote_tool_call) => {
                    remote_tool_call_to_json(remote_tool_call)
                }

                messages::BotEntryKind::Reasoning(reasoning) => {
                    let mut item = serde_json::Map::new();

                    if let Some(text) = reasoning.encrypted_content.as_ref() {
                        item.insert("encrypted_content".to_string(), json!(text));
                    }

                    item.insert("type".to_string(), json!("reasoning"));

                    let mut summary: Vec<Value> = Vec::new();
                    for s in &reasoning.summary {
                        summary.push(json!({ "type": "summary_text", "text": s }));
                    }
                    item.insert("summary".to_string(), json!(summary));

                    let mut content: Vec<Value> = Vec::new();
                    for s in &reasoning.content {
                        content.push(json!({ "type": "reasoning_text", "text": s }));
                    }
                    if !content.is_empty() {
                        item.insert("content".to_string(), json!(content));
                    }

                    if let Some(signature) = &reasoning.signature {
                        item.insert("signature".to_string(), json!(signature));
                    }

                    item
                }
            };

            if let Some(id) = &bot_entry.id {
                item.insert("id".to_string(), json!(id));
            }
            if let Some(completed) = bot_entry.completed {
                item.insert(
                    "status".to_string(),
                    json!(if completed { "completed" } else { "incomplete" }),
                );
            }

            item
        }
    };

    Ok(m)
}

#[instrument(level = "trace", skip_all)]
pub async fn to_openai_messages(history: &[messages::Message]) -> Result<Vec<Value>> {
    let mut out: Vec<serde_json::Map<String, Value>> = Vec::new();

    for message in history {
        out.push(to_openai_message(message).await?);
    }

    Ok(out.into_iter().map(Value::Object).collect::<Vec<_>>())
}

#[instrument(level = "trace", skip_all)]
fn tool_call_to_json(tool_call: &messages::ToolCall) -> Result<serde_json::Map<String, Value>> {
    let mut item = serde_json::Map::new();
    item.insert("type".to_string(), json!("function_call"));
    item.insert("call_id".to_string(), json!(tool_call.remote_call_id));
    item.insert("name".to_string(), json!(tool_call.tool_name));
    item.insert("arguments".to_string(), json!(&tool_call.tool_input));

    if let Some(signature) = &tool_call.signature {
        item.insert("signature".to_string(), json!(signature));
    }

    Ok(item)
}

#[instrument(level = "trace", skip_all)]
fn remote_tool_call_to_json(
    remote_tool_call: &messages::RemoteToolCall,
) -> serde_json::Map<String, Value> {
    match remote_tool_call {
        messages::RemoteToolCall::WebSearch(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("web_search_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("query".to_string(), json!(call.query));
            if !call.results.is_empty() {
                item.insert("results".to_string(), json!(call.results));
            }
            item
        }

        messages::RemoteToolCall::CodeInterpreter(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("code_interpreter_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("code".to_string(), json!(call.code));
            if let Some(output) = &call.output {
                item.insert("output".to_string(), json!(output));
            }
            item
        }

        messages::RemoteToolCall::Computer(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("computer_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("action".to_string(), json!(call.action));
            if let Some(output) = &call.output {
                item.insert("output".to_string(), json!(output));
            }
            item
        }

        messages::RemoteToolCall::ImageGeneration(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("image_generation_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("prompt".to_string(), json!(call.prompt));
            if !call.images.is_empty() {
                item.insert("images".to_string(), json!(call.images));
            }
            item
        }

        messages::RemoteToolCall::LocalShell(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("local_shell_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("command".to_string(), json!(call.command));
            if let Some(output) = &call.output {
                item.insert("output".to_string(), json!(output));
            }
            item
        }

        messages::RemoteToolCall::Mcp(call) => {
            let mut item = serde_json::Map::new();
            item.insert("type".to_string(), json!("mcp_call"));
            item.insert("call_id".to_string(), json!(call.call_id));
            item.insert("server".to_string(), json!(call.server));
            item.insert("method".to_string(), json!(call.method));
            item.insert("arguments".to_string(), json!(call.arguments));
            if let Some(result) = &call.result {
                item.insert("result".to_string(), json!(result));
            }
            item
        }
    }
}
