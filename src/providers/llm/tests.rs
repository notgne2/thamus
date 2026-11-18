use serde_json::{Value, json};

use crate::messages;
use crate::providers::llm::validate;

fn reasoning_message(content: &[&str]) -> messages::Message {
    messages::Message {
        id: 0,
        entry: messages::MessageEntry::BotEntry(messages::BotEntry {
            id: None,
            completed: None,
            kind: messages::BotEntryKind::Reasoning(messages::Reasoning {
                signature: None,
                summary: content
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                content: content
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                encrypted_content: None,
            }),
        }),
    }
}

fn bot_message(content: &str) -> messages::Message {
    messages::Message {
        id: 0,
        entry: messages::MessageEntry::BotEntry(messages::BotEntry {
            id: None,
            completed: None,
            kind: messages::BotEntryKind::BotMessage(messages::BotMessage {
                content: vec![messages::BotMessageContent::Text(
                    messages::BotTextContent {
                        text: content.to_string(),
                        annotations: vec![],
                        logprobs: vec![],
                    },
                )],
                refusal: None,
            }),
        }),
    }
}

fn server_assistant(content: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": content, "annotations": [], "logprobs": [] }],
    })
}

#[tokio::test]
async fn reconstructs_a_plain_assistant_message() {
    validate::validate_reconstruction(
        &bot_message("hello there"),
        &server_assistant("hello there"),
    )
    .await
    .expect("reconstruction should match");
}

#[tokio::test]
async fn detects_a_content_mismatch() {
    let err = validate::validate_reconstruction(
        &bot_message("hello there"),
        &server_assistant("hello theyre"),
    )
    .await
    .expect_err("mismatch should be reported");

    assert!(
        err.to_string().contains("hello there"),
        "error should name the reconstructed value, got: {err}"
    );
}

#[tokio::test]
async fn reconstructs_reasoning_rather_than_dropping_it() {
    validate::validate_reconstruction(
        &reasoning_message(&["thinking hard"]),
        &json!({
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": "thinking hard" }],
            "content": [{ "type": "reasoning_text", "text": "thinking hard" }],
        }),
    )
    .await
    .expect("reasoning should be reconstructed, not dropped");
}

#[tokio::test]
async fn reconstructs_content_and_reasoning_as_separate_response_items() {
    let history = &[reasoning_message(&["thinking hard"]), bot_message("hello")];

    validate::validate_history_round_trip(history)
        .await
        .expect("reasoning and content should each round-trip exactly");
}

#[tokio::test]
async fn reconstructs_a_tool_call() {
    validate::validate_reconstruction(
        &messages::Message {
            id: 0,
            entry: messages::MessageEntry::BotEntry(messages::BotEntry {
                id: None,
                completed: None,
                kind: messages::BotEntryKind::ToolCall(messages::ToolCall {
                    tool_name: "get_weather".to_string(),
                    remote_call_id: "call_1".to_string(),
                    local_call_id: "local_1".to_string(),
                    tool_input: serde_json::to_string(&json!({ "city": "Berlin" })).unwrap(),
                    signature: None,
                }),
            }),
        },
        &json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": r#"{"city":"Berlin"}"#,
        }),
    )
    .await
    .expect("reconstruction should match");
}

#[tokio::test]
async fn reconstructs_a_tool_call_value_verbatim() {
    let input = r#"{  "city":   \n"Berlin"    }"#.to_string();
    validate::validate_reconstruction(
        &messages::Message {
            id: 0,
            entry: messages::MessageEntry::BotEntry(messages::BotEntry {
                id: None,
                completed: None,
                kind: messages::BotEntryKind::ToolCall(messages::ToolCall {
                    tool_name: "get_weather".to_string(),
                    remote_call_id: "call_1".to_string(),
                    local_call_id: "local_1".to_string(),
                    tool_input: input.clone(),
                    signature: None,
                }),
            }),
        },
        &json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": input,
        }),
    )
    .await
    .expect("reconstruction should match");
}

#[tokio::test]
async fn message_reconstruction_survives_key_reordering() {
    validate::validate_reconstruction(
        &bot_message("hi"),
        &json!({
            "content": [{ "text": "hi", "type": "output_text", "annotations": [], "logprobs": [] }],
            "role": "assistant",
            "type": "message",
        }),
    )
    .await
    .expect("key order should not matter");
}

#[tokio::test]
async fn reconstructs_output_item_metadata() {
    let entry = messages::BotEntry {
        id: Some("msg_1".to_string()),
        completed: Some(true),
        kind: messages::BotEntryKind::BotMessage(messages::BotMessage {
            content: vec![messages::BotMessageContent::Text(
                messages::BotTextContent {
                    text: "hi".to_string(),
                    annotations: vec![],
                    logprobs: vec![],
                },
            )],
            refusal: None,
        }),
    };

    validate::validate_output_round_trip(
        &entry,
        &json!({
            "type": "message",
            "id": "msg_1",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hi", "annotations": [], "logprobs": [] }],
        }),
    )
    .await
    .expect("OpenAI item metadata should round-trip");
}
