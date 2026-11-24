use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::agent;
use crate::agent::AgentConfig;
use crate::config::ConfigSnapshot;
use crate::utils::TruncatedReadOutput;
use crate::utils::sandbox::GetName as _;
use crate::utils::sandbox::SandboxPath;
use crate::utils::sandbox::run_in_sandbox;
use crate::utils::sandbox::safe_ensure_sandbox_dir;
use crate::utils::tooling;

#[derive(Deserialize)]
pub struct RunCommandArgs {
    command: String,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    time_limit: Option<u32>,
}

/// Tool that runs a shell command inside an nsjail sandbox.
pub struct RunCommand<C: agent::AgentConfigSource + Send + Sync>(pub C);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for RunCommand<C> {
    fn name(&self) -> &'static str {
        "execute_shell_command"
    }

    fn description(&self, _config_snapshot: &ConfigSnapshot, _sub_config: &AgentConfig) -> String {
        "Executes a shell command on your computer. Share files by embedding them in your response like ![file](path/to/file.ext) relative to your home (/home), or absolute like ![file](/home/my-file.zip). You do NOT need to use this computer when using regular tools, it is separate. ALWAYS PREFER NATIVE TOOLS over running commands. stderr and stdout are both handled together for you. Keep character limits low when possible. NEVER EVER EVER USE THIS TOOL TO RUN A COMMAND IF THERE IS A NATIVE TOOL TO ACCOMPLISH THE SAME THING".to_string()
    }

    async fn parameters(
        &self,
        _config_snapshot: &ConfigSnapshot,
        _sub_config: &AgentConfig,
    ) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "A full command to run in Bash. Commands are auto-fetched from Nix when used. DO NOT check if they are installed. DO NOT attempt to install things. You MUST just use them, and they will be fetched on the fly. ONLY EVER RUN REAL COMMANDS, NEVER USELESS COMMANDS LIKE ECHOING THINGS TO YOURSELF."
                },
                "head_lines": {
                    "type": "integer",
                    "description": "Maximum number of lines to return from the head/start of the output (default 35)"
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Maximum number of lines to return from the tail/end of the output (default 35)"
                },
                "time_limit": {
                    "type": "integer",
                    "description": "Maximum number of seconds to allow the command to run (do not set this excessively high)"
                },
            },
            "required": ["command"]
        })
    }

    async fn call_json(
        &self,
        _config_snapshot: &ConfigSnapshot,
        sub_config: &AgentConfig,
        args: serde_json::Value,
    ) -> Result<(String, Vec<PathBuf>)> {
        let args: RunCommandArgs =
            serde_json::from_value(args).context("parsing execute_shell_command tool arguments")?;

        let mut sandbox_config = sub_config.sandbox.clone();

        if let Some(time_limit) = args.time_limit {
            if time_limit > sandbox_config.time_limit {
                bail!("Time limit exceeds global limit");
            }
            sandbox_config.time_limit = time_limit;
        }

        let head_lines = args.head_lines.unwrap_or(35).clamp(1, 100);
        let tail_lines = args.tail_lines.unwrap_or(35).clamp(1, 100);

        let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
        let sandbox_out_path = SandboxPath::new(
            ["command-output", &format!("{id}.txt")]
                .into_iter()
                .collect::<PathBuf>(),
        );

        safe_ensure_sandbox_dir(
            sandbox_config.root_dir.clone(),
            SandboxPath::from("command-output"),
        )
        .await
        .context("Ensuring sandbox command output dir exists")?;

        let (exit_code, out_res) = run_in_sandbox(
            &sandbox_config,
            head_lines,
            tail_lines,
            sandbox_out_path.clone(),
            &args.command,
        )
        .await
        .context("running command")?;

        let out = match out_res.output {
            TruncatedReadOutput::Full(s) => {
                format!(
                    "[Complete output ({} lines total, less than head+tail)]\n\n{}",
                    s.lines().count(),
                    s,
                )
            }
            TruncatedReadOutput::Truncated(head, tail) => {
                format!(
                    "[Exit code {exit_code} - output ({} bytes) saved to file: ./{}]\n[Head/first {} lines of output]\n\n{}\n\n[Tail/last {} lines of output]\n\n{}",
                    out_res.file_size,
                    sandbox_out_path.display(),
                    head.lines().count(),
                    head,
                    tail.lines().count(),
                    tail
                )
            }
        };

        Ok((out, vec![]))
    }
}
