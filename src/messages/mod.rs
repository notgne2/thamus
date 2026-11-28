use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};

use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt as _};
use tokio_stream::StreamExt as _;
use tracing::{instrument, trace};

use crate::providers::llm;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolingDefinition {
    pub instructions: Option<String>,
    pub tools: Vec<llm::LlmTool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BasePrompt {
    pub instructions: String,
    pub shown_tool_definitions: BTreeMap<String, ToolingDefinition>,
    pub real_tool_definitions: BTreeMap<String, ToolingDefinition>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Prompt {
    pub base: BasePrompt,
    pub history: Vec<Message>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformUserId(pub String);

impl std::fmt::Display for PlatformUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for PlatformUserId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub enum ReceivedAttachmentSource {
    Url(String),
    Data(Vec<u8>),
}

impl fmt::Debug for ReceivedAttachmentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(url) => write!(f, "Url({url})"),
            Self::Data(data) => write!(f, "Data({})", data.len()),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReceivedAttachment {
    pub source: ReceivedAttachmentSource,
    pub ext: String,
}

impl ReceivedAttachment {
    #[instrument(level = "debug", skip_all)]
    pub async fn save(&self, client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("Creating directory at {}", dir.display()))?;

        let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
        let filename = format!("{}.{}", id, self.ext);

        let source = dir.join(&filename);

        trace!("Saving received attachment to {}", source.display());

        let mut save_file = fs::File::create(&source)
            .await
            .with_context(|| format!("Creating file at {}", source.display()))?;

        match &self.source {
            ReceivedAttachmentSource::Url(url) => {
                let mut stream = client.get(url).send().await?.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    save_file.write_all(&chunk?).await?;
                }
                save_file.flush().await?;
            }
            ReceivedAttachmentSource::Data(data) => {
                save_file.write_all(data).await?;
                save_file.flush().await?;
            }
        }

        trace!(
            "Making new attachment struct with source of {}",
            source.display()
        );

        Ok(source)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Mention {
    pub id: PlatformUserId,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PlatformMessage {
    pub id: String,
    pub author_id: PlatformUserId,
    pub author_name: String,
    pub text: String,
    pub mentions: Vec<Mention>,
    pub attachments: Vec<ReceivedAttachment>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MessageContent {
    Text(String),
    Attachment(PathBuf),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SystemEntry {
    ToolResult(ToolResult),

    Compaction(String),
    Infusion(String),
    InstructionDiff(String),
    DefinitionsDiff(String),

    Misc(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BotTextContent {
    pub text: String,
    #[serde(default)]
    pub annotations: Vec<serde_json::Value>,
    #[serde(default)]
    pub logprobs: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BotMessageContent {
    Text(BotTextContent),
    Attachment(PathBuf),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BotMessage {
    pub content: Vec<BotMessageContent>,
    pub refusal: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RemoteToolCall {
    WebSearch(WebSearchCall),
    CodeInterpreter(CodeInterpreterCall),
    Computer(ComputerCall),
    ImageGeneration(ImageGenerationCall),
    LocalShell(LocalShellCall),
    Mcp(McpCall),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WebSearchCall {
    pub call_id: String,
    pub query: String,
    pub results: Vec<WebSearchResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WebSearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CodeInterpreterCall {
    pub call_id: String,
    pub code: String,
    pub output: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ComputerCall {
    pub call_id: String,
    pub action: serde_json::Value,
    pub output: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageGenerationCall {
    pub call_id: String,
    pub prompt: String,
    pub images: Vec<GeneratedImage>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeneratedImage {
    pub url: String,
    pub revised_prompt: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LocalShellCall {
    pub call_id: String,
    pub command: String,
    pub output: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct McpCall {
    pub call_id: String,
    pub server: String,
    pub method: String,
    pub arguments: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BotEntry {
    pub id: Option<String>,
    pub completed: Option<bool>,
    pub kind: BotEntryKind,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BotEntryKind {
    BotMessage(BotMessage),
    ToolCall(ToolCall),
    RemoteToolCall(RemoteToolCall),
    Reasoning(Reasoning),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UserMessage {
    pub content: Vec<MessageContent>,
    pub username: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub remote_call_id: String,
    pub local_call_id: String,
    pub tool_input: String,
    pub signature: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolResult {
    #[serde(default)]
    pub tool_name: String,
    pub remote_call_id: String,
    pub local_call_id: String,
    pub result: String,
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Reasoning {
    pub signature: Option<String>,
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
    pub encrypted_content: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MessageEntry {
    SystemEntry(SystemEntry),
    BotEntry(BotEntry),
    UserMessage(UserMessage),
    Error(String),
}

pub type MessageId = i32;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub entry: MessageEntry,
}
