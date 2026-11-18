use anyhow::Context as _;
use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;

pub async fn tool_save_attachment(
    dir: &Path,
    ext: &str,
) -> Result<(tokio::fs::File, String, PathBuf)> {
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("Creating directory at {}", dir.display()))?;

    let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
    let filename = format!("{id}.{ext}");

    let source = dir.join(&filename);

    let save_file = fs::File::create(&source)
        .await
        .with_context(|| format!("Creating file at {}", source.display()))?;

    Ok((
        save_file,
        "New file created! DO NOT forget to send this to the user. Creating files and not using/sending them is FORBIDDEN".to_string(),
        source,
    ))
}
