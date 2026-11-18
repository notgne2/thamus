use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::instrument;

use super::to_openai;
use crate::messages;

#[instrument(level = "trace", skip_all)]
pub async fn validate_history_round_trip(history: &[messages::Message]) -> Result<()> {
    let openai = to_openai::to_openai_messages(history)
        .await
        .context("Encoding OpenAI history")?;

    for (index, (message, item)) in history.iter().zip(openai.iter()).enumerate() {
        if let messages::MessageEntry::BotEntry(_) = &message.entry {
            let entries = super::from_openai::to_bot_entry(item)
                .with_context(|| format!("Decoding OpenAI history item {index}"))?;
            validate_output_round_trip(&entries, item)
                .await
                .with_context(|| format!("Validating OpenAI history item {index}"))?;
        }
    }

    Ok(())
}

#[instrument(level = "trace", skip_all)]
pub async fn validate_output_round_trip(
    bot_entry: &messages::BotEntry,
    expected: &Value,
) -> Result<()> {
    let message = messages::Message {
        id: 0,
        entry: messages::MessageEntry::BotEntry(bot_entry.clone()),
    };
    let from_entry = to_openai::to_openai_message(&message)
        .await
        .context("Re-encoding OpenAI output item")?;

    let normalized_from_entry = normalize(&serde_json::Value::Object(from_entry));
    let normalized_expected = unjunky(&normalize(expected));

    if normalized_from_entry != normalized_expected {
        bail!(
            "OpenAI output round-trip mismatch:\nfrom entry:\n{normalized_from_entry}\nexpected:\n{normalized_expected}\n\nentry:\n{bot_entry:?}",
        );
    }

    Ok(())
}

#[cfg(test)]
#[instrument(level = "trace", skip_all)]
pub async fn validate_reconstruction(message: &messages::Message, expected: &Value) -> Result<()> {
    let from_entry = to_openai::to_openai_message(message)
        .await
        .context("Re-encoding history entry")?;

    let normalized_from_entry = normalize(&serde_json::Value::Object(from_entry));
    let normalized_expected = normalize(expected);

    if normalized_from_entry != normalized_expected {
        bail!(
            "OpenAI input reconstruction mismatch:\nfrom entry:\n{normalized_from_entry}\nexpected:\n{normalized_expected}",
        );
    }

    Ok(())
}

fn unjunky(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut map = map.clone();

            // one API encountered included `"summary":[]` on message responses
            if map.get("type") == Some(&Value::String("message".to_string()))
                && map.get("summary") != Some(&Value::Array(vec![]))
            {
                map.remove("summary");
            }

            Value::Object(map)
        }
        scalar => scalar.clone(),
    }
}

#[instrument(level = "trace", skip_all)]
fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries = map
                .iter()
                .map(|(key, value)| (key.clone(), normalize(value)))
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));

            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        scalar => scalar.clone(),
    }
}
