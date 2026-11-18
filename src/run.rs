use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use backon::ExponentialBuilder;
use backon::Retryable;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::select;
use tokio::sync::{Notify, RwLock};
use tokio::task;
use tracing::warn;
use tracing::{debug, error, info, instrument, trace};

use crate::config::ConfigSnapshot;
use crate::instance::InstanceProps;
use crate::{
    CommonResources,
    config::Config,
    instance::Instance,
    platforms::{
        Platform, PlatformInstance,
        implementations::{discord::DiscordPlatform, discord_voice::DiscordVoicePlatform},
    },
};

pub struct Thamus;
impl Thamus {
    #[instrument(level = "info", skip_all)]
    pub async fn run(
        common: CommonResources,
        config_snapshot: &ConfigSnapshot,
        platforms: Vec<Arc<dyn Platform>>,
        platform_instances: Vec<(Arc<dyn PlatformInstance>, InstanceProps)>,
    ) -> anyhow::Result<()> {
        trace!("Creating data directory");

        tokio::fs::create_dir_all("data")
            .await
            .context("Creating data directory")?;

        debug!("Starting platforms");

        for platform in platforms {
            let platform = platform.clone();
            let common = common.clone();
            let config_snapshot = config_snapshot.clone();

            tokio::spawn(async move {
                let x = || Self::run_platform(&common, &config_snapshot, &platform);
                // never returns. no max times.
                let _ = x
                    .retry(ExponentialBuilder::new().without_max_times())
                    .notify(|err: &anyhow::Error, dur: Duration| {
                        error!("Platform error: {err:?}\n\nRestarting platform after {dur:?}");
                    })
                    .await;
            });
        }

        for (platform_instance, instance_props) in platform_instances {
            Self::launch_instance(common.clone(), platform_instance, instance_props);
        }

        Ok(())
    }

    async fn run_platform(
        common: &CommonResources,
        config_snapshot: &ConfigSnapshot,
        platform: &Arc<dyn Platform>,
    ) -> Result<!> {
        let run_instances = tokio::spawn({
            let platform = platform.clone();
            let common = common.clone();

            async move { Self::run_platform_instances(&common, &platform).await }
        });

        let start_platform = platform.run(config_snapshot);

        select! {
            tr = run_instances => {
                let Err(err) = tr.context("Joining platform instance runner task")?;
                Err(err).context("Running instances for platform")
            },
            r = start_platform => {
                let Err(err) = r;
                Err(err).context("Running platform")
            }
        }
    }

    async fn run_platform_instances(
        common: &CommonResources,
        platform: &Arc<dyn Platform>,
    ) -> Result<!> {
        loop {
            let (platform_instance, props) = platform
                .instance_receiver()
                .recv()
                .await
                .context("Instance receiver closed?")?;

            let common = common.clone();

            Self::launch_instance(common, platform_instance, props);
        }
    }

    fn launch_instance(
        common: CommonResources,
        platform_instance: Arc<dyn PlatformInstance>,
        props: InstanceProps,
    ) {
        task::spawn({
            async move {
                let x = || Instance::run(platform_instance.clone(), common.clone(), props.clone());
                let _ = x
                    .retry(ExponentialBuilder::new().without_max_times())
                    .notify(|err: &anyhow::Error, dur: Duration| {
                        error!("Instance error: {err:?}\n\nRestarting instance after {dur:?}");
                    })
                    .await;
            }
        });
    }
}

fn load_sqlite_vec() {
    // SAFETY: `sqlite3_vec_init` is the standard SQLite extension entrypoint,
    // `int sqlite3_vec_init(sqlite3*, char**, const sqlite3_api_routines*)`. The
    // crate declares it with no parameters but `sqlite3_auto_extension` wants
    // that real three-argument signature, so the transmute bridges the
    // declaration mismatch. SQLite only ever calls an auto-extension entrypoint
    // with exactly those three arguments, so this is the same call the crate's
    // own test makes.
    unsafe {
        let entry: unsafe extern "C" fn(
            *mut libsqlite3_sys::sqlite3,
            *mut *mut std::ffi::c_char,
            *const libsqlite3_sys::sqlite3_api_routines,
        ) -> std::ffi::c_int = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());

        libsqlite3_sys::sqlite3_auto_extension(Some(entry));
    }
}

#[instrument(level = "info", skip_all)]
pub async fn run_bot(database: PathBuf) -> Result<()> {
    info!("Preparing Thamus...");

    let initial_config = Config::load_full().await.context("loading configuration")?;
    let config_snapshot = Arc::new(initial_config);
    let shared_config = Arc::new(RwLock::new(Arc::clone(&config_snapshot)));
    info!("Configuration loaded");

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install rustls crypto provider: {e:?}"))?;

    {
        let shared_config = Arc::clone(&shared_config);
        task::spawn(async move { Config::watch(shared_config).await });
    }

    load_sqlite_vec();

    let options = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));

    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .context("Opening database")?;

    info!("Connected to SQLite, running migrations");

    sqlx::migrate!()
        .run(&pool)
        .await
        .context("Running migrations")?;

    info!("DB ready");

    crate::db::clear_slots(&pool)
        .await
        .context("Clearing in use slots")?;

    let common = CommonResources {
        pool,
        config: Arc::clone(&shared_config),
        http: reqwest::Client::new(),
        lock_notifier: Arc::new(Notify::new()),
    };

    tokio::spawn({
        let common = common.clone();
        async move {
            if let Err(e) = crate::server::serve(&common).await {
                error!("Server error: {}", e);
            }
        }
    });

    let mut platforms: Vec<Arc<dyn Platform>> = Vec::new();

    if config_snapshot.platforms.discord.is_some() {
        platforms.push(Arc::new(DiscordPlatform::new(Arc::clone(&shared_config))));
    }

    if config_snapshot.platforms.discord_voice.is_some() {
        platforms.push(Arc::new(DiscordVoicePlatform::new(&common)));
    }

    info!("Starting Thamus...");

    Thamus::run(common, &config_snapshot, platforms, vec![])
        .await
        .context("Starting Thamus")?;

    info!("Thamus is running. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c()
        .await
        .context("listening for Ctrl+C")?;

    info!("Shutdown signal received, ending Thamus");

    Ok(())
}
