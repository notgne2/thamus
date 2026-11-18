#![recursion_limit = "256"]
#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::sync::Arc;

use tracing::error;
use tracing_subscriber::EnvFilter;

pub(crate) mod agent;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod config_rules;
pub(crate) mod db;
pub(crate) mod instance;
pub(crate) mod mem;
pub(crate) mod messages;
pub(crate) mod platforms;
pub(crate) mod providers;
pub(crate) mod run;
pub(crate) mod server;
pub(crate) mod tools;
pub(crate) mod utils;

#[derive(Clone)]
pub(crate) struct CommonResources {
    pub pool: sqlx::Pool<sqlx::Sqlite>,
    pub config: config::SharedConfig,
    pub http: reqwest::Client,
    pub lock_notifier: Arc<tokio::sync::Notify>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("thamus=info")),
        )
        .init();

    if let Err(err) = cli::run().await {
        error!("{err:?}");
        Err(err)
    } else {
        Ok(())
    }
}
