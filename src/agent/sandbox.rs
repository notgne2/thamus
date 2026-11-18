use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use tokio::fs::{self, OpenOptions};
use tokio::io::{BufReader, BufWriter};
use tracing::{instrument, trace};

use crate::agent::AgentConfig;
use crate::config::ConfigSnapshot;
use crate::utils::sandbox::{SandboxPath, safe_ensure_sandbox_dir, safe_open_sandbox_file};

#[instrument(level = "debug", skip_all, fields(attachment_path = %attachment_path.display()))]
pub async fn make_sandbox_from_attachment(
    sub_config: &AgentConfig,
    attachment_path: &Path,
) -> Result<SandboxPath> {
    safe_ensure_sandbox_dir(
        sub_config.sandbox.root_dir.clone(),
        SandboxPath::from("attachments"),
    )
    .await?;

    let filename = attachment_path
        .file_name()
        .context("No file name on saved attachment?")?
        .to_str()
        .context("Saved attachment filename has dodgy file name")?;
    let sandbox_save_path = SandboxPath::new(PathBuf::from("attachments").join(filename));

    let sandbox_file = safe_open_sandbox_file(
        sub_config.sandbox.root_dir.clone(),
        sandbox_save_path.clone(),
        true,
    )
    .await
    .context("Opening sandbox file to copy received attachment to")?;

    let mut reader = BufReader::new(
        fs::File::open(&attachment_path)
            .await
            .context("Opening attachment path for copying contents to sandbox")?,
    );
    let mut writer = BufWriter::new(sandbox_file);
    tokio::io::copy(&mut reader, &mut writer).await?;

    Ok(sandbox_save_path)
}

#[instrument(level = "debug", skip_all, fields(sandbox_path = %sandbox_path.display()))]
pub async fn make_attachment_from_sandbox(
    config: &ConfigSnapshot,
    sub_config: &AgentConfig,
    sandbox_path: SandboxPath,
) -> Result<PathBuf> {
    let ext = sandbox_path
        .extension()
        .unwrap_or_else(|| OsStr::new("idk"));
    let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
    let mut filename = OsString::from(id);
    filename.push(".");
    filename.push(ext);

    let attachment_path = config.storage.attachments_dir.join(PathBuf::from(filename));

    trace!(
        "Saving sandbox path {} to attachment path {}",
        sandbox_path.display(),
        attachment_path.display()
    );

    let sandbox_file =
        safe_open_sandbox_file(sub_config.sandbox.root_dir.clone(), sandbox_path, false)
            .await
            .context("Opening sandbox file for markdown embed to create attachment")?;

    let attachment_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&attachment_path)
        .await
        .context("Creating attachment file to store markdown attachment")?;

    let mut reader = BufReader::new(sandbox_file);
    let mut writer = BufWriter::new(attachment_file);

    tokio::io::copy(&mut reader, &mut writer).await?;

    Ok(attachment_path)
}
