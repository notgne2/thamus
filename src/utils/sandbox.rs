use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task;
use tracing::trace;

use crate::utils::TruncatedReadRes;

#[derive(Debug, Clone)]
pub struct SandboxPath(PathBuf);

impl SandboxPath {
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }
    pub fn from<T: Into<PathBuf>>(path: T) -> Self {
        Self(path.into())
    }
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    pub fn extension(&self) -> Option<&OsStr> {
        self.0.extension()
    }

    fn absolutify(self) -> Self {
        if self.0.is_absolute() {
            self
        } else {
            Self::new(PathBuf::from("/home").join(&self.0))
        }
    }

    fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }
    fn parent(&self) -> Option<&Path> {
        self.0.parent()
    }
    fn path(&self) -> &Path {
        &self.0
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
    trace!(
        "Ensuring sandbox dir: in_root={:?}, path={:?}",
        in_root, path
    );

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
    trace!(
        "Reading sandbox dir: in_root={:?}, path={:?}",
        in_root, path
    );

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

pub async fn safe_open_sandbox_file(
    in_root: PathBuf,
    mut path: SandboxPath,
    write: bool,
) -> Result<fs::File> {
    // trace!(
    //     "Opening sandbox file: in_root={:?}, path={:?}, write={}",
    //     in_root, path, write
    // );

    path = path.absolutify();

    let file = task::spawn_blocking(move || {
        let root_file = std::fs::File::open(&in_root).context("opening sandbox root directory")?;
        let fd = rustix::fs::openat2(
            root_file,
            path.path(),
            (if write {
                rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE
            } else {
                rustix::fs::OFlags::RDONLY
            }) | rustix::fs::OFlags::CLOEXEC,
            if write {
                rustix::fs::Mode::from_bits(0o644).unwrap()
            } else {
                rustix::fs::Mode::empty()
            },
            rustix::fs::ResolveFlags::NO_MAGICLINKS | rustix::fs::ResolveFlags::IN_ROOT,
        )
        .with_context(|| {
            format!(
                "Safely opening sandbox file path: {} in real-world root {} for {}",
                path.display(),
                in_root.display(),
                if write { "writing" } else { "reading" },
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
    head_size: usize,
    tail_size: usize,
    path: SandboxPath,
) -> Result<TruncatedReadRes> {
    let mut file: fs::File =
        safe_open_sandbox_file(config.root_dir.clone(), path.clone(), false).await?;

    crate::utils::truncated_read(&mut file, head_size, tail_size)
        .await
        .context("Reading output file")
}

pub async fn run_in_sandbox(
    config: &SandboxConfig,

    head_size: usize,
    tail_size: usize,

    out_path: SandboxPath,

    command: &str,
) -> Result<(i32, TruncatedReadRes)> {
    let mut cmd = sandboxify_command(config, command, &HashMap::new()).await?;

    let out_file: fs::File =
        safe_open_sandbox_file(config.root_dir.clone(), out_path.clone(), true).await?;

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

    let read_output = truncated_read(config, head_size, tail_size, out_path).await?;

    let exit_code = status.code().unwrap_or(-1);

    Ok((exit_code, read_output))
}
