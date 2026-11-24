//! Configuration management
//! Loads application settings from config.toml using the config crate.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use futures::StreamExt as _;
use inotify::{Inotify, WatchMask};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::{fs, sync::RwLock};
use tokio_stream::wrappers::ReadDirStream;
use tracing::{error, info, instrument};

use crate::utils::sandbox::{GetName, safe_sandbox_file_reader};
use crate::{
    agent, config_rules, instance, platforms, providers, server,
    utils::sandbox::{SandboxPath, safe_ensure_sandbox_dir, safe_read_sandbox_dir},
};

/// Configures data storage
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Where to store the sqlite file
    pub sqlite_path: PathBuf,
    /// Where to store save-only attachments
    pub attachments_dir: PathBuf,
    /// Where to store voice clips
    pub voice_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            sqlite_path: PathBuf::from("./data/db.sqlite"),
            attachments_dir: PathBuf::from("./data/attachments"),
            voice_dir: PathBuf::from("./data/voice"),
        }
    }
}

/// Config for the whole thing!
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Providers (LLM, image gen, etc)
    pub providers: providers::ProvidersConfig,
    /// Instance (the agent you chat with)
    pub instance: instance::InstanceConfig,
    /// Agent (the internal logic for responses and agent functionality)
    #[serde(default)]
    agent: agent::AgentConfig,
    /// Platforms (where you can chat with it)
    #[serde(default)]
    pub platforms: platforms::PlatformsConfig,
    /// Where to store data
    #[serde(default)]
    pub storage: StorageConfig,
    /// The server for making tools work
    #[serde(default)]
    pub server: server::ServerConfig,
    /// Selfserve config rules
    #[serde(default)]
    #[allow(clippy::struct_field_names)]
    pub config_filter: config_rules::RulesConfig,

    selfserve_error: Option<String>,
}

pub type SharedConfig = Arc<RwLock<Arc<Config>>>;
pub type ConfigSnapshot = Arc<Config>;

pub async fn snapshot_config(shared_config: &SharedConfig) -> ConfigSnapshot {
    Arc::clone(&*shared_config.read().await)
}

impl Config {
    pub fn selfserve_error(&self) -> Option<&str> {
        self.selfserve_error.as_deref()
    }

    pub const fn default_agent(&self) -> &agent::AgentConfig {
        &self.agent
    }

    async fn read_files_to_the_config(
        the_config: &mut serde_json::Value,
        files: &mut [(impl GetName, impl AsyncRead + Unpin)],
    ) -> Result<()> {
        for (path, file_reader) in files {
            let mut content = String::new();
            file_reader
                .read_to_string(&mut content)
                .await
                .with_context(|| format!("Reading config file {:?}", path.file_name()))?;

            let value: serde_json::Value = match path
                .extension()
                .with_context(|| {
                    format!("File {:?} is missing a file extension", path.file_name())
                })?
                .to_str()
                .with_context(|| {
                    format!("Bad config file extension in file: {:?}", path.file_name())
                })? {
                "json" | "json5" | "jsonc" => json5::from_str(&content)
                    .with_context(|| format!("Parsing JSON config file: {:?}", path.file_name()))?,
                "ron" => ron::from_str(&content)
                    .with_context(|| format!("Parsing RON config file: {:?}", path.file_name()))?,
                "toml" => toml::from_str(&content)
                    .with_context(|| format!("Parsing TOML config file: {:?}", path.file_name()))?,
                _ => continue,
            };

            json_patch::merge(the_config, &value);
        }

        Ok(())
    }

    #[instrument(level = "debug")]
    pub async fn load_main() -> Result<Self> {
        let mut files = Self::get_files()
            .await
            .context("Getting config file list")?;

        let mut the_config = serde_json::Value::Object(serde_json::Map::new());
        Self::read_files_to_the_config(&mut the_config, files.as_mut_slice())
            .await
            .context("Reading config files")?;

        serde_json::from_value(the_config).context("Deserializing config")
    }

    #[instrument(level = "debug", skip_all)]
    async fn get_selfserve_files(&self) -> Result<Vec<(SandboxPath, impl AsyncRead)>> {
        let root = &self.default_agent().sandbox.root_dir;

        let selfserve_config_dir = SandboxPath::from("settings");

        safe_ensure_sandbox_dir(root.clone(), selfserve_config_dir.clone())
            .await
            .context("Ensuring selfserve settings directory")?;

        let mut settings_read_dir = safe_read_sandbox_dir(root.clone(), selfserve_config_dir)
            .await
            .context("Reading selfserve settings directory")?;

        let mut files = Vec::new();
        while let Some(entry) = settings_read_dir.recv().await {
            let file_reader = safe_sandbox_file_reader(root.clone(), entry.clone())
                .await
                .context("Safely opening selfserve settings file")?;
            files.push((entry, file_reader));
        }

        Ok(files)
    }

    #[instrument(level = "debug", skip_all)]
    async fn load_selfserve(&self) -> Result<Self> {
        let mut selfserve_files = self
            .get_selfserve_files()
            .await
            .context("Getting selfserve files")?;

        let mut selfserve_config = serde_json::Value::Object(serde_json::Map::new());
        Self::read_files_to_the_config(&mut selfserve_config, selfserve_files.as_mut_slice())
            .await
            .context("Reading selfserve config files")?;

        {
            let violations =
                config_rules::find_config_violations(&selfserve_config, &self.config_filter);
            if !violations.is_empty() {
                bail!("Config rule violations found: {violations:?}");
            }
        }

        let mut the_config = serde_json::to_value(self).context("Deserializing own config")?;
        json_patch::merge(&mut the_config, &selfserve_config);

        serde_json::from_value(the_config).context("Deserializing merged config as config")
    }

    #[instrument(level = "debug")]
    async fn get_files() -> Result<Vec<(PathBuf, fs::File)>> {
        let settings_read_dir = fs::read_dir("./settings")
            .await
            .context("reading settings directory")?;
        let mut settings_read_dir_stream = ReadDirStream::new(settings_read_dir);

        let mut files: Vec<(PathBuf, fs::File)> = Vec::new();

        while let Some(entry_r) = settings_read_dir_stream.next().await {
            let entry = entry_r.context("reading settings file entry")?;

            let path = entry.path();

            let file = fs::File::open(&path)
                .await
                .context("opening settings file")?;

            files.push((path, file));
        }

        Ok(files)
    }

    #[instrument(level = "debug")]
    pub async fn load_full() -> Result<Self> {
        let mut config = Self::load_main().await.context("Loading main config")?;

        match config.load_selfserve().await {
            Ok(selfserve_config) => {
                config = selfserve_config;
            }
            Err(err) => {
                error!("Failed to load selfserve config: {err:#?}");
                config.selfserve_error = Some(format!("{err:#?}"));
            }
        }

        Ok(config)
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn watch(shared_config: Arc<RwLock<Arc<Self>>>) -> Result<()> {
        let inotify = Inotify::init().expect("Failed to initialize inotify");

        let mut watches = inotify.watches();
        watches.add(
            "./settings",
            WatchMask::CREATE | WatchMask::DELETE | WatchMask::MODIFY | WatchMask::MOVE,
        )?;

        {
            let config_snapshot = snapshot_config(&shared_config).await;
            let selfserve_config_file = config_snapshot
                .default_agent()
                .sandbox
                .root_dir
                .join("home")
                .join("config.json");
            watches.add(
                &selfserve_config_file,
                WatchMask::CREATE | WatchMask::DELETE | WatchMask::MODIFY | WatchMask::MOVE,
            )?;
        }

        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer)?;

        while let Some(_event_or_error) = stream.next().await {
            info!("Loading config after change detected");

            match Self::load_full().await {
                Ok(config) => {
                    let mut c = shared_config.write().await;
                    *c = Arc::new(config);
                    drop(c);
                }
                Err(err) => {
                    error!("Failed to load config: {err:#?}");
                }
            }

            info!("Config loaded successfully");
        }

        Ok(())
    }
}
