use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task;
use tracing::instrument;

use crate::utils::TruncatedReadRes;

pub trait GetName {
    fn extension(&self) -> Option<&OsStr>;
    fn file_name(&self) -> Option<&OsStr>;
    fn display_file_name(&self) -> Option<std::ffi::os_str::Display<'_>>;
    fn display(&self) -> std::path::Display<'_>;
}

#[derive(Debug, Clone)]
pub struct SandboxPath(PathBuf);

impl SandboxPath {
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }
    pub fn from<T: Into<PathBuf>>(path: T) -> Self {
        Self(path.into())
    }

    fn absolutify(self) -> Self {
        if self.0.is_absolute() {
            self
        } else {
            Self::new(PathBuf::from("/home").join(&self.0))
        }
    }

    fn parent(&self) -> Option<&Path> {
        self.0.parent()
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl GetName for SandboxPath {
    fn extension(&self) -> Option<&OsStr> {
        self.0.extension()
    }

    fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    fn display_file_name(&self) -> Option<std::ffi::os_str::Display<'_>> {
        Some(self.0.file_name()?.display())
    }
}

impl GetName for Path {
    fn extension(&self) -> Option<&OsStr> {
        Path::extension(self)
    }

    fn display(&self) -> std::path::Display<'_> {
        Path::display(self)
    }

    fn file_name(&self) -> Option<&OsStr> {
        Path::file_name(self)
    }

    fn display_file_name(&self) -> Option<std::ffi::os_str::Display<'_>> {
        Some(Path::file_name(self)?.display())
    }
}

impl GetName for PathBuf {
    fn extension(&self) -> Option<&OsStr> {
        Path::extension(self)
    }

    fn display(&self) -> std::path::Display<'_> {
        Path::display(self)
    }

    fn file_name(&self) -> Option<&OsStr> {
        Path::file_name(self)
    }

    fn display_file_name(&self) -> Option<std::ffi::os_str::Display<'_>> {
        Some(Path::file_name(self)?.display())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub root_dir: PathBuf,
    pub nix_packages: Vec<String>,
    pub time_limit: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("./data/computer"),
            nix_packages: [
                "nixpkgs#bash",
                "nixpkgs#coreutils",
                "nixpkgs#nix-index",
                "nixpkgs#comma",
                "nixpkgs#curl",
                "nixpkgs#wget",
                "nixpkgs#cacert",
                "nixpkgs#ffmpeg-full",
                "nixpkgs#yt-dlp",
                "nixpkgs#ghostscript",
                "nixpkgs#which",
                "nixpkgs#nodejs",
                "nixpkgs#jq",
                "nixpkgs#imagemagick",
            ]
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>(),
            time_limit: 120,
        }
    }
}

pub async fn safe_ensure_sandbox_dir(in_root: PathBuf, mut path: SandboxPath) -> Result<()> {
    path = path.absolutify();

    task::spawn_blocking(move || {
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            bail!("Path is missing filename or parent");
        };

        let root_file = std::fs::File::open(&in_root).context("opening sandbox root directory")?;
        let fd = rustix::fs::openat2(
            root_file,
            parent,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_MAGICLINKS | rustix::fs::ResolveFlags::IN_ROOT,
        )
        .with_context(|| {
            format!(
                "Safely creating dir {} inside {} in real-world root {}",
                name.display(),
                parent.display(),
                in_root.display()
            )
        })?;

        let file = std::fs::File::from(fd);

        match rustix::fs::mkdirat(file, name, rustix::fs::Mode::from_bits(0o755).unwrap()) {
            Ok(()) => {}
            Err(e) => match e.kind() {
                std::io::ErrorKind::AlreadyExists => {}
                _ => anyhow::bail!("Failed to create directory: {e}"),
            },
        }

        anyhow::Ok(())
    })
    .await
    .context("spawning blocking file read task")?
    .context("opening sandbox file to read")?;

    Ok(())
}

pub async fn safe_read_sandbox_dir(
    in_root: PathBuf,
    mut path: SandboxPath,
) -> Result<mpsc::UnboundedReceiver<SandboxPath>> {
    path = path.absolutify();

    let (tx, rx) = mpsc::unbounded_channel();

    task::spawn_blocking(move || {
        let root_file = std::fs::File::open(&in_root).context("opening sandbox root directory")?;
        let fd = rustix::fs::openat2(
            root_file,
            path.path(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_MAGICLINKS | rustix::fs::ResolveFlags::IN_ROOT,
        )
        .with_context(|| {
            format!(
                "Safely opening sandbox dir: in_root={:?}, path={:?}",
                in_root.display(),
                path.display()
            )
        })?;

        let mut dir = rustix::fs::Dir::new(fd).context("Using safely opened dir fd as dir")?;

        while let Some(entry_r) = dir.read() {
            let entry = entry_r.context("Handling dir entry")?;

            let file_name_str = OsStr::from_bytes(entry.file_name().to_bytes());

            if file_name_str == OsStr::new(".") || file_name_str == OsStr::new("..") {
                continue;
            }

            let sub_path = SandboxPath::new(path.path().join(file_name_str));

            tx.send(sub_path)
                .context("Sending path from sandbox directory")?;
        }

        anyhow::Ok(())
    })
    .await
    .context("Spawning blocking file read task")?
    .context("Opening sandbox file to read")?;

    Ok(rx)
}

pub async fn safe_sandbox_file_reader_uncapped(
    in_root: PathBuf,
    mut path: SandboxPath,
) -> Result<fs::File> {
    path = path.absolutify();

    let file = task::spawn_blocking(move || {
        let root_file = std::fs::File::open(&in_root).context("opening sandbox root directory")?;
        let fd = rustix::fs::openat2(
            root_file,
            path.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_MAGICLINKS | rustix::fs::ResolveFlags::IN_ROOT,
        )
        .with_context(|| {
            format!(
                "Safely opening sandbox file path: {} in real-world root {} to safely read",
                path.display(),
                in_root.display(),
            )
        })?;
        let file = std::fs::File::from(fd);
        anyhow::Ok(fs::File::from_std(file))
    })
    .await
    .context("spawning blocking file read task")?
    .context("opening sandbox file to read")?;

    Ok(file)
}

pub async fn safe_sandbox_file_reader(
    in_root: PathBuf,
    path: SandboxPath,
) -> Result<impl AsyncRead> {
    let file = safe_sandbox_file_reader_uncapped(in_root, path).await?;
    Ok(file.take(crate::MAX_SANDBOX_READ as u64))
}

pub async fn safe_sandbox_file_writer(
    in_root: PathBuf,
    mut path: SandboxPath,
    reset: bool,
) -> Result<fs::File> {
    path = path.absolutify();

    let file = task::spawn_blocking(move || {
        let root_file = std::fs::File::open(&in_root).context("opening sandbox root directory")?;
        let fd = rustix::fs::openat2(
            root_file,
            path.path(),
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_bits(0o644).unwrap(),
            rustix::fs::ResolveFlags::NO_MAGICLINKS | rustix::fs::ResolveFlags::IN_ROOT,
        )
        .with_context(|| {
            format!(
                "Safely opening sandbox file path: {} in real-world root {} for writing",
                path.display(),
                in_root.display(),
            )
        })?;
        let mut file = std::fs::File::from(fd);
        if reset {
            file.set_len(0).context("Truncating sandbox file")?;
            file.seek(SeekFrom::Start(0))
                .context("Seeking to start of truncated sandbox file")?;
        } else {
            file.seek(SeekFrom::End(0))
                .context("Seeking to end of non-truncated sandbox file")?;
        }

        anyhow::Ok(fs::File::from_std(file))
    })
    .await
    .context("spawning blocking file write task")?
    .context("opening sandbox file to write")?;

    Ok(file)
}

pub async fn sandboxify_command(
    config: &SandboxConfig,
    command: &str,
    env: &HashMap<String, String>,
) -> Result<Command> {
    let home_dir = [config.root_dir.as_path(), Path::new("home")]
        .iter()
        .collect::<PathBuf>();

    let nix_path = {
        let mut nix_cmd = tokio::process::Command::new("nix");
        nix_cmd.env("NIXPKGS_ALLOW_INSECURE", "1");
        nix_cmd.env("NIXPKGS_ALLOW_UNFREE", "1");
        nix_cmd
            .arg("--extra-experimental-features")
            .arg("nix-command flakes");
        nix_cmd.arg("shell");
        nix_cmd.arg("--impure");
        for pkg in &config.nix_packages {
            nix_cmd.arg(pkg);
        }
        nix_cmd.arg("-c").arg("bash").arg("-c").arg("echo $PATH");
        let nix_path_output = nix_cmd.output().await.context("resolving nix shell PATH")?;

        if nix_path_output.stdout.is_empty() {
            bail!(
                "nix shell returned empty PATH, stderr: {}",
                String::from_utf8_lossy(&nix_path_output.stderr)
            );
        }

        String::from_utf8_lossy(&nix_path_output.stdout)
            .trim()
            .to_string()
    };

    let ssl_cert_file = {
        let cacert_output = tokio::process::Command::new("nix")
            .arg("--extra-experimental-features")
            .arg("nix-command flakes")
            .arg("eval")
            .arg("--raw")
            .arg("nixpkgs#cacert.outPath")
            .output()
            .await
            .context("resolving cacert path")?;
        format!(
            "{}/etc/ssl/certs/ca-bundle.crt",
            String::from_utf8_lossy(&cacert_output.stdout).trim()
        )
    };

    let mut cmd = tokio::process::Command::new("bwrap");

    cmd.arg("--unshare-pid");
    cmd.arg("--unshare-uts");
    cmd.arg("--die-with-parent");
    cmd.arg("--dev").arg("/dev");
    cmd.arg("--proc").arg("/proc");
    cmd.arg("--tmpfs").arg("/tmp");
    // cmd.arg("--tmpfs").arg("/run");

    cmd.arg("--ro-bind").arg("/nix").arg("/nix");

    cmd.arg("--ro-bind").arg("/bin").arg("/bin");
    cmd.arg("--ro-bind").arg("/usr").arg("/usr");

    if tokio::fs::metadata(std::path::Path::new("/nix/var/nix/daemon-socket"))
        .await
        .is_ok()
    {
        cmd.arg("--bind")
            .arg("/nix/var/nix/daemon-socket")
            .arg("/nix/var/nix/daemon-socket");
    }

    if tokio::fs::metadata(std::path::Path::new("/etc/nix"))
        .await
        .is_ok()
    {
        cmd.arg("--ro-bind").arg("/etc/nix").arg("/etc/nix");
    }

    if tokio::fs::metadata(std::path::Path::new("/etc/resolv.conf"))
        .await
        .is_ok()
    {
        cmd.arg("--ro-bind")
            .arg("/etc/resolv.conf")
            .arg("/etc/resolv.conf");
    }

    cmd.arg("--bind").arg(&home_dir).arg("/home");

    cmd.arg("--clearenv");
    cmd.arg("--setenv").arg("HOME").arg("/home");
    cmd.arg("--setenv").arg("USER").arg("user");
    cmd.arg("--setenv").arg("PATH").arg(&nix_path);
    cmd.arg("--setenv").arg("SSL_CERT_FILE").arg(&ssl_cert_file);
    cmd.arg("--setenv")
        .arg("NIX_SSL_CERT_FILE")
        .arg(&ssl_cert_file);

    for (key, value) in env {
        cmd.arg("--setenv").arg(key).arg(value);
    }

    cmd.arg("--chdir").arg("/home");
    cmd.arg("--hostname").arg("sandbox");

    let internal_script = format!(
        r#"mkdir -p ~/.config/nix
if [ ! -f ~/.config/nix/nix.conf ]; then
printf '%s\n' 'experimental-features = nix-command flakes' > ~/.config/nix/nix.conf.tmp
mv ~/.config/nix/nix.conf.tmp ~/.config/nix/nix.conf
fi
if [ ! -s "$HOME/.cache/nix-index/files" ]; then
mkdir -p ~/.cache/nix-index
_nix_idx_file="index-$(uname -m | sed 's/^arm64$/aarch64/')-$(uname | tr A-Z a-z)"
wget -O ~/.cache/nix-index/"$_nix_idx_file" "https://github.com/nix-community/nix-index-database/releases/latest/download/$_nix_idx_file"
ln -sf "$_nix_idx_file" ~/.cache/nix-index/files
fi
mkdir -p ~/.local/bin
if [ ! -f ~/.local/bin/comma-picker ]; then
printf '#!/bin/sh\nhead -1\n' > ~/.local/bin/comma-picker
chmod +x ~/.local/bin/comma-picker
fi
export COMMA_PICKER=~/.local/bin/comma-picker
command_not_found_handle() {{
    # `,` is a command which pulls binaries on the fly from Nix,
    # we want to mute its stderr though
    COMMAND_PATH=$(, -x "$1" 2>/dev/null)
    shift
    $COMMAND_PATH "$@"
}}
cd ~
{command}
"#
    );

    cmd.arg("bash").arg("-c").arg(&internal_script);

    Ok(cmd)
}

pub async fn truncated_read(
    config: &SandboxConfig,
    head_lines: usize,
    tail_lines: usize,
    path: SandboxPath,
) -> Result<TruncatedReadRes> {
    let mut file = safe_sandbox_file_reader_uncapped(config.root_dir.clone(), path.clone()).await?;
    let file_size = file
        .metadata()
        .await
        .context("Reading file metadata")?
        .len();

    crate::utils::truncated_read(&mut file, file_size, head_lines, tail_lines)
        .await
        .context("Reading output file")
}

pub async fn run_in_sandbox(
    config: &SandboxConfig,

    head_lines: usize,
    tail_lines: usize,

    out_path: SandboxPath,

    command: &str,
) -> Result<(i32, TruncatedReadRes)> {
    let mut cmd = sandboxify_command(config, command, &HashMap::new()).await?;

    let out_file =
        safe_sandbox_file_writer(config.root_dir.clone(), out_path.clone(), true).await?;

    let std_file: std::fs::File = out_file.into_std().await;
    let fd_out: OwnedFd = std_file.into();
    let fd_err: OwnedFd = fd_out.try_clone()?;

    let out_pipe = std::process::Stdio::from(fd_out);
    let err_pipe = std::process::Stdio::from(fd_err);

    cmd.stdin(std::process::Stdio::null())
        .stdout(out_pipe)
        .stderr(err_pipe);

    let status = tokio::time::timeout(
        Duration::from_secs(u64::from(config.time_limit)),
        cmd.status(),
    )
    .await
    .context("Timeout running command in sandbox")?
    .context("running bwrap/command")?;

    let read_output = truncated_read(config, head_lines, tail_lines, out_path).await?;

    let exit_code = status.code().unwrap_or(-1);

    Ok((exit_code, read_output))
}

#[instrument(level = "debug", skip_all, fields(attachment_path = %attachment_path.display()))]
pub async fn make_sandbox_from_attachment(
    sandbox_config: &SandboxConfig,
    attachment_path: &Path,
) -> Result<SandboxPath> {
    safe_ensure_sandbox_dir(
        sandbox_config.root_dir.clone(),
        SandboxPath::from("attachments"),
    )
    .await?;

    let filename = attachment_path
        .file_name()
        .context("No file name on saved attachment?")?
        .to_str()
        .context("Saved attachment filename has dodgy file name")?;
    let sandbox_save_path = SandboxPath::new(PathBuf::from("attachments").join(filename));

    let sandbox_writer = safe_sandbox_file_writer(
        sandbox_config.root_dir.clone(),
        sandbox_save_path.clone(),
        true,
    )
    .await
    .context("Opening sandbox file writer to copy attachment to")?;

    let mut attachment_reader = BufReader::new(
        fs::File::open(&attachment_path)
            .await
            .context("Opening attachment path for copying contents to sandbox")?,
    );
    let mut buffered_sandbox_writer = BufWriter::new(sandbox_writer);
    tokio::io::copy(&mut attachment_reader, &mut buffered_sandbox_writer)
        .await
        .context("Copying attachment to sandbox")?;
    buffered_sandbox_writer
        .flush()
        .await
        .context("Flushing sandbox file")?;

    Ok(sandbox_save_path)
}

#[instrument(level = "debug", skip_all, fields(sandbox_path = %sandbox_path.display()))]
pub async fn make_attachment_from_sandbox(
    attachments_dir: &Path,
    sandbox_config: &SandboxConfig,
    sandbox_path: SandboxPath,
) -> Result<PathBuf> {
    let ext = sandbox_path
        .extension()
        .unwrap_or_else(|| OsStr::new("idk"));
    let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
    let mut filename = OsString::from(id);
    filename.push(".");
    filename.push(ext);

    let attachment_path = attachments_dir.join(PathBuf::from(filename));

    let sandbox_reader = safe_sandbox_file_reader(sandbox_config.root_dir.clone(), sandbox_path)
        .await
        .context("Opening sandbox file for markdown embed to create attachment")?;

    let attachment_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&attachment_path)
        .await
        .context("Creating attachment file to store markdown attachment")?;

    let mut attachment_writer = BufWriter::new(attachment_file);
    let mut buffered_sandbox_reader = BufReader::new(sandbox_reader);

    tokio::io::copy(&mut buffered_sandbox_reader, &mut attachment_writer)
        .await
        .context("Copying sandbox file to attachment file")?;
    attachment_writer
        .flush()
        .await
        .context("Flushing attachment file")?;

    Ok(attachment_path)
}
