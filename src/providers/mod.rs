use serde::{Deserialize, Serialize};

pub mod embed;
pub mod generate;
pub mod llm;

/// Configures providers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersConfig {
    /// LLM configuration
    pub llm: llm::LlmConfig,
    /// Embedding configuration
    #[serde(default)]
    pub embed: Option<embed::EmbedConfig>,
    /// URL for a stable-diffusion.cpp server
    #[serde(default)]
    pub sd_cpp_uri: Option<String>,
    /// URL for an audio.cpp server
    #[serde(default)]
    pub audio_cpp_uri: Option<String>,
}
