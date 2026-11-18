use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::CommonResources;
use crate::config::snapshot_config;
use crate::providers::generate;
use crate::utils::tooling;

use super::utils::tool_save_attachment;

#[derive(Deserialize)]
pub struct GenerateSpeechArgs {
    voice: String,
    words: String,
    model: String,
}

pub struct GenerateSpeech(pub CommonResources);

#[async_trait]
impl tooling::BotTool for GenerateSpeech {
    fn name(&self) -> &'static str {
        "generate_speech"
    }

    fn description(&self) -> String {
        "Generates speech (NEVER songs). This is JUST a pure text-to-speech which uses custom voices. NO ACTIONS, SOUNDS, SECTIONS, OR SONGS".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        let config_snapshot = snapshot_config(&self.0.config).await;

        let voice_names = config_snapshot
            .instance
            .voice
            .voices
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>();

        let mut voice_description =
            "What voice to use for the speech, your options are:\n\n".to_string();

        for voice in &config_snapshot.instance.voice.voices {
            voice_description.push_str(&voice.name);
            if let Some(char_card) = &voice.char_card {
                voice_description.push_str(": ");
                voice_description.push_str(char_card);
            }
            voice_description.push_str("\n\n");
        }

        voice_description.push_str("NEVER directly quote or even adapt reference quotes, only use them as general guidance on how the character may talk");

        json!({
            "type": "object",
            "properties": {
                "model": {
                  "type": "string",
                  "enum": ["omnivoice", "chatterbox"],
                  "description": "The TTS model to use for voice cloning/generation. MUST ALWAYS be omnivoice unless a user EXPLICITLY asks for inferior model"
                },
                "voice": {
                  "type": "string",
                  "enum": voice_names,
                  "description": voice_description
                },
                "words": {
                    "type": "string",
                    "description": "SPOKEN words to speak out loud. This must not contain annotations of any kind, only spoken words, with the sole exception of [laughter], which is supported, but no other expressions, annotations, or formatting are"
                },
            },
            "required": ["voice", "words", "model"],
        })
    }
    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let config_snapshot = snapshot_config(&self.0.config).await;

        let Some(api_url) = &config_snapshot.providers.audio_cpp_uri else {
            bail!("Generation has been disabled");
        };

        let args: GenerateSpeechArgs =
            serde_json::from_value(args).context("parsing generate_speech tool arguments")?;

        let voice = config_snapshot
            .instance
            .voice
            .voices
            .iter()
            .find(|v| v.name == args.voice)
            .cloned()
            .context("voice not found")?;

        let (mut save_target, msg, attachment) =
            tool_save_attachment(&config_snapshot.storage.attachments_dir, "wav").await?;

        generate::generate_speech(
            &self.0.http,
            api_url,
            &args.model,
            &voice.ref_file,
            &voice.ref_text,
            &args.words,
            &mut save_target,
        )
        .await
        .context("generating speech")?;

        Ok((msg, vec![attachment]))
    }
}
