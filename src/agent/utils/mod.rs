use regex::Regex;
use std::sync::LazyLock;
use tracing::instrument;

use crate::{messages, utils::sandbox::SandboxPath};

pub mod json_diff;
pub mod simple_diff;

#[instrument(level = "trace", skip(bot_message))]
pub fn extract_markdown_attachments(bot_message: &messages::BotMessage) -> Vec<SandboxPath> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\(([^)]+)\)").unwrap());
    let mut paths = Vec::new();

    for content in &bot_message.content {
        if let messages::BotMessageContent::Text(t) = &content {
            for capture in RE.captures_iter(&t.text) {
                let path_match = capture
                    .get(1)
                    .expect("markdown attachment extractor regex should have capture group 1");

                let extracted_path_text = path_match.as_str().trim();
                let extracted_path_text = extracted_path_text
                    .strip_prefix("file://")
                    .unwrap_or(extracted_path_text);
                let sandbox_path = SandboxPath::from(extracted_path_text);

                paths.push(sandbox_path);
            }
        }
    }

    paths
}
