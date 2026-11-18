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

async fn convert_wav_to_ogg(input: &str, output: &str) -> Result<()> {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-vn")
        .arg("-c:a")
        .arg("libopus")
        .arg("-b:a")
        .arg("128k")
        .arg(output);

    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().context("Running FFMPEG")?;

    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "ffmpeg failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

#[derive(Deserialize)]
pub struct GenerateSongArgs {
    description: String,
    lyrics: String,
    duration: u16,
}

pub struct GenerateSong(pub CommonResources);

#[async_trait]
impl tooling::BotTool for GenerateSong {
    fn name(&self) -> &'static str {
        "generate_song"
    }

    fn description(&self) -> String {
        "Generates an song".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Include ONLY musical/technical aspects like instruments, sounds, vocals, production, genre. (e.g., 'tight snare with a punchy 808 bass and a sharp synth, deep male voice with high female backing vocals, warm tones, combining jazz and rap style'). NEVER include lyrics, vibes, or descriptions of the song content, only TECHNICAL DESCRIPTIONS. NEVER any vibey/moody words (i.e. dark, uplifting, comedic, bouncy, other non-technical descriptions)"
                },
                "lyrics": {
                    "type": "string",
                    "description": "The full lyrics of the song, annotating sections with [Verse], [Chorus], etc"
                },
                "duration": {
                    "type": "number",
                    "description": "OVER-ESTIMATE the length of the song in seconds. DO NOT under-estimate. Take the lyrics, BPM, automatic instrumental sections, intro, and outro into account"
                },
            },
            "required": ["description", "lyrics", "duration"],
        })
    }
    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let config = snapshot_config(&self.0.config).await;

        let Some(api_url) = &config.providers.audio_cpp_uri else {
            bail!("Generation has been disabled");
        };

        let (mut save_target, msg, attachment) =
            tool_save_attachment(&config.storage.attachments_dir, "wav").await?;

        let args: GenerateSongArgs =
            serde_json::from_value(args).context("parsing generate_song tool arguments")?;

        generate::generate_song(
            &self.0.http,
            api_url,
            &args.description,
            &args.lyrics,
            args.duration,
            &mut save_target,
        )
        .await
        .context("generating song")?;

        let ogg_path = attachment.with_extension("ogg");

        let ogg_path_str = ogg_path
            .as_path()
            .to_str()
            .context("ogg path makes an invalid string")?;

        let original_path_str = attachment
            .as_path()
            .to_str()
            .context("original wav path makes an invalid string")?;

        convert_wav_to_ogg(original_path_str, ogg_path_str)
            .await
            .context("Converting wav to ogg")?;

        Ok((msg, vec![ogg_path]))
    }
}
