//! `cargo xtask dist`: builds the static release binaries that install.sh
//! (`apps/site/install.sh`) installs, as the archive
//! `target/dist/piqueld-<version>-x86_64-linux.tar.gz` and its `SHA256SUMS`.
//! The release workflow attaches both to the GitHub release; see
//! docs/dev/releasing.md. With `PIQUELD_BUILD_CHANNEL`, the binaries report
//! that channel, and it also writes the channel's feed,
//! `target/dist/<channel>.json`, which the workflow publishes.
//!
//! The binaries link musl and OpenSSL statically. Building them needs
//! `musl-gcc`, from Debian and Ubuntu's `musl-tools`, for the C code of
//! OpenSSL, SQLite and ring.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use piqueld_client::version::{Channel, Feed, REPOSITORY_URL, Version};

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// The Rust target the release binaries build for.
const TARGET: &str = "x86_64-unknown-linux-musl";

/// The release archive of one version, with what it holds: each file's path
/// in the workspace, and its name in the archive.
struct Archive {
    /// `target/dist`, which holds the archive and its checksums.
    dist: PathBuf,
    /// The archive's top-level directory, such as
    /// `piqueld-0.1.0-x86_64-linux`.
    name: String,
    /// Each file to archive: its path in the workspace and its name in the archive.
    files: [(PathBuf, &'static str); 4],
}

impl Archive {
    /// The archive of `version`, from the workspace's built binaries.
    fn new(workspace: &Workspace, version: &str) -> Self {
        let root = workspace.root();
        let binaries = root.join("target").join(TARGET).join("release");
        Self {
            dist: root.join("target/dist"),
            name: format!("piqueld-{version}-x86_64-linux"),
            files: [
                (binaries.join("piqueld"), "piqueld"),
                (binaries.join("piquelctl"), "piquelctl"),
                (root.join("apps/piqueld/piqueld.service"), "piqueld.service"),
                (root.join("LICENSE"), "LICENSE"),
            ],
        }
    }

    /// The archive's file name.
    fn file(&self) -> String {
        format!("{}.tar.gz", self.name)
    }

    /// Replaces `target/dist` with the archive and its `SHA256SUMS`, and
    /// returns the archive's SHA-256.
    fn write(&self) -> Result<String> {
        if self.dist.exists() {
            fs::remove_dir_all(&self.dist)
                .with_context(|| format!("remove {}", self.dist.display()))?;
        }
        let staging = self.dist.join(&self.name);
        fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
        for (from, name) in &self.files {
            fs::copy(from, staging.join(name))
                .with_context(|| format!("copy {}", from.display()))?;
        }
        let archive = self.file();
        self.run(Command::new("tar").args([
            "--create",
            "--gzip",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--file",
            &archive,
            &self.name,
        ]))?;
        fs::remove_dir_all(&staging).with_context(|| format!("remove {}", staging.display()))?;
        let sums = self.run(Command::new("sha256sum").arg(&archive))?;
        let sha256 = sums
            .split_whitespace()
            .next()
            .with_context(|| format!("no checksum in sha256sum's output: {sums}"))?
            .to_owned();
        let path = self.dist.join("SHA256SUMS");
        fs::write(&path, sums + "\n").with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} and SHA256SUMS",
            self.dist.join(&archive).display()
        );
        Ok(sha256)
    }

    /// Writes `channel`'s feed, which names this archive as its newest build,
    /// whose GitHub release is tagged `v<version>`.
    fn write_feed(&self, channel: Channel, version: Version, sha256: String) -> Result<()> {
        let tag = format!("v{}", version.number);
        let feed = Feed {
            notes_url: format!("{REPOSITORY_URL}/releases/tag/{tag}"),
            archive_url: format!("{REPOSITORY_URL}/releases/download/{tag}/{}", self.file()),
            archive_sha256: sha256,
            version,
        };
        let path = self.dist.join(channel.feed_file());
        let json = serde_json::to_string_pretty(&feed).context("serialize the feed")?;
        fs::write(&path, json + "\n").with_context(|| format!("write {}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    }

    /// Runs `command` in `target/dist`, failing unless it succeeds, and
    /// returns its trimmed output.
    fn run(&self, command: &mut Command) -> Result<String> {
        let name = command.get_program().to_string_lossy().into_owned();
        let output = command
            .current_dir(&self.dist)
            .output()
            .with_context(|| format!("run {name}"))?;
        checked_stdout(&name, output)
    }
}

/// Builds the binaries for [`TARGET`], with the embedded dashboard, then
/// writes their [`Archive`], and the feed of the channel
/// `PIQUELD_BUILD_CHANNEL` names, unless it is unset or empty. The binaries
/// report `HEAD`'s commit, and when it was made (`piqueld_core::version`),
/// even when the tree has changes.
pub(crate) fn run(workspace: &Workspace) -> Result<ExitCode> {
    let channel = match std::env::var("PIQUELD_BUILD_CHANNEL") {
        Ok(name) if name.is_empty() => None,
        Ok(name) => Some(
            Channel::parse(&name)
                .with_context(|| format!("PIQUELD_BUILD_CHANNEL={name} isn't stable or nightly"))?,
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error).context("read PIQUELD_BUILD_CHANNEL"),
    };
    let root = workspace.root();
    let commit = Workspace::git(root, &["rev-parse", "HEAD"])?;
    let committed_at = Workspace::git(root, &["log", "-1", "--format=%ct", "HEAD"])?;
    let number = workspace.release_version(channel)?;
    let mut cargo = workspace.cargo();
    cargo.env_remove("PIQUELD_BUILD_CHANNEL");
    if let Some(channel) = channel {
        cargo.env("PIQUELD_BUILD_CHANNEL", channel.as_str());
    }
    let status = cargo
        .env("PIQUELD_BUILD_VERSION", &number)
        .env("PIQUELD_BUILD_COMMIT", &commit)
        .env("PIQUELD_BUILD_COMMIT_TIME", &committed_at)
        .args(["build", "--release", "--locked", "--target", TARGET])
        .args(["--package", "piqueld", "--package", "piquelctl"])
        .args(["--bin", "piqueld", "--bin", "piquelctl"])
        .args(["--features", "embedded-ui"])
        .status()
        .context("run cargo build")?;
    if !status.success() {
        return Ok(exit_code(status));
    }
    let archive = Archive::new(workspace, &number);
    let sha256 = archive.write()?;
    if let Some(channel) = channel {
        let committed_at: i64 = committed_at
            .parse()
            .with_context(|| format!("parse the commit time {committed_at}"))?;
        let version = Version {
            number,
            commit: Some(commit),
            committed_at_ms: Some(committed_at * 1000),
            channel: Some(channel.into()),
        };
        archive.write_feed(channel, version, sha256)?;
    }
    Ok(ExitCode::SUCCESS)
}

impl Workspace {
    /// The version a build on `channel` gets: the workspace version, or for
    /// a nightly, the next minor version with today's UTC date, such as
    /// `0.3.0-nightly.20261125` after 0.2.x, which sorts after every 0.2
    /// release and before 0.3.0.
    fn release_version(&self, channel: Option<Channel>) -> Result<String> {
        let version = self.version()?;
        if channel != Some(Channel::Nightly) {
            return Ok(version);
        }
        let mut parts = version.splitn(3, '.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            anyhow::bail!("the workspace version {version} isn't major.minor.patch");
        };
        let minor: u64 = minor
            .parse()
            .with_context(|| format!("parse the minor version of {version}"))?;
        let output = Command::new("date")
            .args(["-u", "+%Y%m%d"])
            .output()
            .context("run date")?;
        let date = checked_stdout("date", output)?;
        Ok(format!("{major}.{}.0-nightly.{date}", minor + 1))
    }
}
