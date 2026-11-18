use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::Empty;

use crate::CommonResources;
use crate::agent;
use crate::config::snapshot_config;
use crate::providers::generate;
use crate::utils::sandbox::SandboxPath;
use crate::utils::sandbox::safe_sandbox_file_reader;
use crate::utils::tooling;

use super::utils::tool_save_attachment;

#[derive(Deserialize)]
pub struct GenerateImageArgs {
    prompt: String,
    resolution: Option<String>,
}

pub struct GenerateImage<C: agent::AgentConfigSource + Send + Sync>(pub CommonResources, pub C);

fn parse_resolution(resolution: Option<&str>) -> anyhow::Result<Option<(u32, u32)>> {
    let loose_res_strs = match resolution {
        Some(r) => Some(
            r.split_once('x')
                .context("resolution must be in the format 'WIDTHxHEIGHT'")?,
        ),
        _ => None,
    };

    let loose_res = match loose_res_strs {
        Some((w, h)) => Some((
            w.parse::<u32>().context("width must be a number")?,
            h.parse::<u32>().context("height must be a number")?,
        )),
        _ => None,
    };

    Ok(loose_res)
}

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for GenerateImage<C> {
    fn name(&self) -> &'static str {
        "generate_image"
    }

    fn description(&self) -> String {
        "Generates an image locally".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Describe the image in detail, DESCRIBING ALL CHARACTERS. The generaotr DOES NOT KNOW WHAT PEOPLE LOOK LIKE and it DOES NOT KNOW THEIR NAMES"
                },
                "resolution": {
                    "type": "string",
                    "description": "Image resolution (used for aspect ratio, automatically scaled)"
                }
            },
            "required": ["prompt"]
        })
    }
    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let config = snapshot_config(&self.0.config).await;

        let Some(api_url) = &config.providers.sd_cpp_uri else {
            bail!("Generation has been disabled");
        };

        let (mut save_target, msg, attachment) =
            tool_save_attachment(&config.storage.attachments_dir, "png").await?;

        let args: GenerateImageArgs =
            serde_json::from_value(args).context("parsing generate_image tool arguments")?;

        let loose_res =
            parse_resolution(args.resolution.as_deref()).context("failed to parse resolution")?;

        generate::generate_image(
            &self.0.http,
            api_url,
            (&mut []) as &mut [Empty],
            &args.prompt,
            loose_res,
            &mut save_target,
        )
        .await
        .context("generating image")?;

        Ok((msg, vec![attachment]))
    }
}

#[derive(Deserialize)]
pub struct EditImageArgs {
    prompt: String,
    inputs: Vec<String>,
    resolution: Option<String>,
}

pub struct EditImage<C: agent::AgentConfigSource + Send + Sync>(pub CommonResources, pub C);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for EditImage<C> {
    fn name(&self) -> &'static str {
        "edit_image"
    }

    fn description(&self) -> String {
        "Edits image(s) using a prompt. Like generate_image, but takes existing image(s) to edit into 1".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Describe your changes. This can be something like \"put the lizard in the bedroom\" (2 references provided), \"make weird al black\", etc. \"First\" and \"second\" are meaningless to this tool, it does not know the order of inputs. The filenames are meaningless to this tool, do not use them. You MUST address the CONTENT of the images, not their filenames or order"
                },
                "resolution": {
                    "type": "string",
                    "description": "Image resolution. Default of first image's resolution"
                },
                "inputs": {
                    "type": "array",
                    "description": "List of image paths to edit (first is used for aspect ratio/resolution). These cannot be referenced in the prompt by filename or order, only by content",
                    "items": {
                        "type": "string",
                        "description": "Path to an image you want to edit"
                    }
                }
            },
            "required": ["prompt", "inputs"]
        })
    }
    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let config = snapshot_config(&self.0.config).await;

        let Some(api_url) = &config.providers.sd_cpp_uri else {
            bail!("Generation has been disabled");
        };

        let (mut save_target, msg, attachment) =
            tool_save_attachment(&config.storage.attachments_dir, "png").await?;

        let args: EditImageArgs =
            serde_json::from_value(args).context("parsing edit_image tool arguments")?;

        let mut sandbox_in_files = Vec::new();

        let sub_config = &*self.1.get_config().await;

        for sandbox_input_path in args.inputs {
            let sandbox_in_file = safe_sandbox_file_reader(
                sub_config.sandbox.root_dir.clone(),
                SandboxPath::from(sandbox_input_path),
            )
            .await?;
            sandbox_in_files.push(sandbox_in_file);
        }

        let loose_res =
            parse_resolution(args.resolution.as_deref()).context("failed to parse resolution")?;

        generate::generate_image(
            &self.0.http,
            api_url,
            &mut sandbox_in_files[..],
            &args.prompt,
            loose_res,
            &mut save_target,
        )
        .await
        .context("editing image")?;

        Ok((msg, vec![attachment]))
    }
}
