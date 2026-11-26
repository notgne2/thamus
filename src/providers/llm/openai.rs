use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource as _;
use futures::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, error, instrument, trace, warn};

#[derive(Debug, PartialEq, Eq)]
pub enum StreamMessage {
    Partial(Value, Option<Usage>),
    Complete(Value, Option<Usage>),
    End(Usage),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub cached_tokens: u32,
    pub tokens_generated: u32,
}

#[instrument(level = "trace", skip_all)]
pub async fn input_tokens(
    client: &reqwest::Client,
    base_url: &str,
    api_token: &str,
    body: Value,
) -> Result<u64> {
    let url = format!("{base_url}/responses/input_tokens");
    let response = client
        .post(&url)
        .bearer_auth(api_token)
        .json(&body)
        .send()
        .await?;

    let status = response.status();

    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .context("Failed to read error response body")?;

        match serde_json::from_str::<serde_json::Value>(&error_body) {
            Ok(json) => bail!("Server returned {status} with JSON error:\n{json:?}"),
            Err(_e) => {
                bail!("Server returned {status} with non-JSON error:\n{error_body}");
            }
        };
    }

    let result: serde_json::Value = response.json().await?;
    let tokens_v = result
        .get("input_tokens")
        .context("No input_tokens field in response")?;
    let tokens = tokens_v.as_u64().context("input_tokens is not a number")?;

    Ok(tokens)
}

#[derive(Debug, Error)]
pub enum CallError {
    #[error("Request error: {0}")]
    Request(reqwest::Error),
    #[error("Server returned error {0}: {1:?}")]
    Server(reqwest::StatusCode, serde_json::Map<String, Value>),
    #[error("Error field missing in error response {0}: {1}")]
    ErrorMissing(reqwest::StatusCode, serde_json::Value),
    #[error("Error field not an object in error response {0}: {1}")]
    ErrorFieldNotObject(reqwest::StatusCode, serde_json::Value),
    #[error("Unparsable server error {0}: {1}")]
    UnparsableServer(reqwest::StatusCode, String),
    #[error("Bad body error {0}")]
    BadBody(reqwest::StatusCode, reqwest::Error),
}

#[instrument(level = "trace", skip_all)]
pub async fn stream(
    client: &reqwest::Client,
    base_url: &str,
    api_token: &str,
    body: Value,
) -> std::result::Result<impl futures::Stream<Item = Result<StreamMessage>>, CallError> {
    let url = format!("{base_url}/responses");

    trace!("Calling OpenAI stream endpoint: {}", url);

    let response = client
        .post(&url)
        .bearer_auth(api_token)
        .json(&body)
        .send()
        .await
        .map_err(CallError::Request)?;

    let status = response.status();
    trace!("Response status: {}", status);

    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .map_err(|e| CallError::BadBody(status, e))?;

        let mut json: serde_json::Value = serde_json::from_str(&error_body)
            .map_err(|e| CallError::UnparsableServer(status, e.to_string()))?;

        let Some(error_v) = json.get_mut("error") else {
            return Err(CallError::ErrorMissing(status, json));
        };

        let serde_json::Value::Object(error) = error_v.take() else {
            return Err(CallError::ErrorFieldNotObject(status, json));
        };

        return Err(CallError::Server(status, error));
    }

    let mut sse = response.bytes_stream().eventsource();

    let messages = async_stream::try_stream! {
        let mut acc = Accumulator::default();

        while let Some(event) = sse.next().await {
            let event = event.context("Reading LLM SSE stream item")?;

            if event.event == "keepalive" {
                trace!("keepalive event");
                continue;
            }

            let serde_json::Value::Object(chunk) =
                serde_json::from_str(&event.data).context("Parsing OpenAI stream chunk")?
            else {
                Err(anyhow!("Chunk is not an object: {}", event.data))?;
                unreachable!("Used ? on an Err (in a try_stream! block)");
            };

            let usage = read_usage_from_timings(&chunk)?;

            if let Some(message) = acc.ingest(chunk, usage)? {
                yield message;
            }
        }

        for message in acc.finish() {
            yield message;
        }
    };

    Ok(messages)
}

#[instrument(level = "trace", skip_all)]
fn read_usage_from_timings(chunk: &serde_json::Map<String, Value>) -> Result<Option<Usage>> {
    let Some(timings) = chunk.get("timings") else {
        return Ok(None);
    };

    let read = |key: &str| -> Result<u32> {
        let n = timings.get(key).and_then(Value::as_u64).unwrap_or(0);
        u32::try_from(n).with_context(|| format!("Token count for {key} overflows u32: {n}"))
    };

    Ok(Some(Usage {
        input_tokens: read("prompt_n")?,
        cached_tokens: read("cache_n")?,
        tokens_generated: read("predicted_n")?,
    }))
}

#[derive(Default)]
struct Accumulator {
    items: std::collections::HashMap<String, PartialItem>,
}

struct PartialItem {
    text: String,
}

impl Accumulator {
    #[instrument(level = "trace", skip_all)]
    fn ingest(
        &mut self,
        mut chunk: serde_json::Map<String, Value>,
        usage: Option<Usage>,
    ) -> Result<Option<StreamMessage>> {
        let event_type = chunk.get("type").and_then(Value::as_str);

        match event_type {
            Some(t @ ("response.output_text.delta" | "response.reasoning_text.delta")) => {
                let item_id = chunk
                    .get("item_id")
                    .and_then(Value::as_str)
                    .context("response.output_text.delta missing item_id")?;
                let delta = chunk
                    .get("delta")
                    .and_then(Value::as_str)
                    .context("response.output_text.delta missing delta")?;

                let item = self
                    .items
                    .entry(item_id.to_string())
                    .or_insert_with(|| PartialItem {
                        text: String::new(),
                    });

                item.text.push_str(delta);

                return Ok(Some(StreamMessage::Partial(
                    serde_json::json!({
                        "type": if t == "response.reasoning_text.delta" { "reasoning" } else { "message" },
                        "content": [{
                            "type": if t == "response.reasoning_text.delta" { "reasoning_text" } else { "output_text" },
                            "text": item.text.clone()
                        }],
                        "role": "assistant"
                    }),
                    usage,
                )));
            }

            Some("response.output_item.done") => {
                if let Some(item) = chunk.remove("item") {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        self.items.remove(id);
                    }

                    return Ok(Some(StreamMessage::Complete(item.clone(), usage)));
                }
            }

            Some(
                "response.created"
                | "response.in_progress"
                | "response.output_item.added"
                | "response.content_part.added"
                | "response.function_call_arguments.delta"
                | "response.output_text.done"
                | "response.content_part.done",
            ) => {}

            Some("error" | "server_error") => {
                error!("Error inside the response stream? {:#?}", chunk);
                bail!("Error inside the response stream: {chunk:#?}");
            }

            Some("response.completed") => {
                trace!("Got response.completed event");

                let response = chunk
                    .get("response")
                    .and_then(|u| u.as_object())
                    .context("Response missing from response.completed event")?;

                let usage = response
                    .get("usage")
                    .and_then(|u| u.as_object())
                    .context("Usage missing from response.completed response object")?;

                let input_tokens = u32::try_from(
                    usage
                        .get("input_tokens")
                        .context("Usage missing input_tokens")?
                        .as_u64()
                        .context("input_tokens not a number")?,
                )
                .context("input_tokens too large")?;

                let input_tokens_details = usage
                    .get("input_tokens_details")
                    .and_then(|u| u.as_object())
                    .context("Usage missing input_tokens_details")?;

                let cached_tokens = u32::try_from(
                    input_tokens_details
                        .get("cached_tokens")
                        .context("Input token details missing cached_tokens")?
                        .as_u64()
                        .context("cached_tokens not a number")?,
                )
                .context("cached_tokens too large")?;

                let tokens_generated = u32::try_from(
                    usage
                        .get("output_tokens")
                        .context("Usage missing output_tokens")?
                        .as_u64()
                        .context("tokens_generated not a number")?,
                )
                .context("tokens_generated too large")?;

                let timings = Usage {
                    input_tokens: input_tokens - cached_tokens,
                    cached_tokens,
                    tokens_generated,
                };

                trace!("Got timings from response.completed event: {timings:?}");

                return Ok(Some(StreamMessage::End(timings)));
            }

            _ => {
                debug!("Not yet handled event type: {:?}", event_type);
            }
        }

        Ok(None)
    }

    #[instrument(level = "trace", skip_all)]
    fn finish(self) -> Vec<StreamMessage> {
        if !self.items.is_empty() {
            warn!(
                "Stream ended with {} incomplete items still in accumulator",
                self.items.len()
            );
        }
        Vec::new()
    }
}
