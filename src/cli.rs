use anyhow::Context as _;
use bpaf::Bpaf;
use tracing::instrument;

use crate::{
    config::Config,
    run,
    tools::{self, HistoryCmd, MemoryCmd, history_cmd, memory_cmd},
};

#[derive(Debug, Clone, Bpaf)]
#[bpaf(options, generate(cli_options))]
pub enum Cli {
    /// Run the bot.
    #[bpaf(command)]
    Run {},

    /// Inspect stored conversation history.
    #[bpaf(command("history"))]
    History {
        #[bpaf(external(history_cmd))]
        cmd: HistoryCmd,
    },

    /// Manage stored memories.
    #[bpaf(command("memory"))]
    Memory {
        #[bpaf(external(memory_cmd))]
        cmd: MemoryCmd,
    },
}

#[instrument(level = "info", skip_all)]
pub async fn run() -> anyhow::Result<()> {
    let cli = cli_options().run();

    let config = Config::load_main().await.context("loading configuration")?;

    match cli {
        Cli::Run {} => run::run_bot(config.storage.sqlite_path.clone()).await,

        Cli::History { cmd } => tools::run_history(cmd, &config.server).await,

        Cli::Memory { cmd } => tools::run_memory(cmd, &config.server).await,
    }
}
