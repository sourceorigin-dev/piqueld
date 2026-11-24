//! `piqueld update`: installs a published build on a host the install script
//! set up, and rolls it back, as root. It reports each step as a `tracing`
//! event.
//!
//! Each build lives in its own directory, such as
//! `/usr/local/lib/piqueld/0.3.0`. The `current` link names the one that
//! runs, which `/usr/local/bin/piqueld` and `piquelctl` resolve through, and
//! `previous` the one it replaced, for rollback. A database a newer version
//! updated is never handed to an older one ([`Updater::rollback`]).

use crate::backup::Backups;
use anyhow::{Context, Result, bail, ensure};
use piqueld_config::DaemonConfig;
use piqueld_core::version::REPOSITORY_URL;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The service the install script installs.
const SERVICE: &str = "piqueld";
/// Time the restarted daemon may take to answer, migrations included.
const START_TIMEOUT: Duration = Duration::from_mins(3);
/// Time between attempts to reach the restarted daemon.
const START_POLL: Duration = Duration::from_secs(2);
/// Time the daemon may take to answer, longer than a check of its channel.
const DAEMON_TIMEOUT: Duration = Duration::from_mins(1);
/// Time a download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

/// Where builds, the links to them and the service's unit are.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The directory holding each build, `current` and `previous`.
    builds: PathBuf,
    /// The directory linking `piqueld` and `piquelctl` to the current build.
    bin: PathBuf,
    /// The systemd unit, replaced by each build's own.
    unit: PathBuf,
}

impl Layout {
    /// The install script's: builds in `/usr/local/lib/piqueld`, linked
    /// from `/usr/local/bin`, and `/etc/systemd/system/piqueld.service`.
    #[must_use]
    pub fn script() -> Self {
        Self {
            builds: PathBuf::from("/usr/local/lib/piqueld"),
            bin: PathBuf::from("/usr/local/bin"),
            unit: PathBuf::from("/etc/systemd/system/piqueld.service"),
        }
    }

    /// The directory of the build `version`.
    fn build(&self, version: &str) -> PathBuf {
        self.builds.join(version)
    }

    /// The build a link of [`Self::builds`], `current` or `previous`, names.
    fn linked(&self, link: &str) -> Option<String> {
        fs::read_link(self.builds.join(link))
            .ok()
            .and_then(|target| target.file_name()?.to_str().map(str::to_owned))
    }

    /// The version the build `version`'s daemon reports, which names the
    /// backups it takes.
    fn reported_version(&self, version: &str) -> Result<String> {
        let daemon = self.build(version).join("piqueld");
        let output = Command::new(&daemon)
            .arg("--version")
            .output()
            .with_context(|| format!("run {} --version", daemon.display()))?;
        let printed = String::from_utf8_lossy(&output.stdout);
        Ok(printed
            .trim()
            .strip_prefix("piqueld ")
            .with_context(|| format!("{} printed no version: {printed}", daemon.display()))?
            .to_owned())
    }

    /// Points the link `link` at the build `version`, atomically.
    fn link(&self, link: &str, version: &str) -> Result<()> {
        let path = self.builds.join(link);
        let staged = self.builds.join(format!(".{link}.new"));
        remove(&staged)?;
        std::os::unix::fs::symlink(version, &staged)
            .with_context(|| format!("link {}", staged.display()))?;
        fs::rename(&staged, &path).with_context(|| format!("replace {}", path.display()))
    }

    /// Links `piqueld` and `piquelctl` to the current build, replacing the
    /// files an older install script copied there.
    fn link_binaries(&self) -> Result<()> {
        for binary in ["piqueld", "piquelctl"] {
            let path = self.bin.join(binary);
            let staged = self.bin.join(format!(".{binary}.new"));
            remove(&staged)?;
            std::os::unix::fs::symlink(self.builds.join("current").join(binary), &staged)
                .with_context(|| format!("link {}", staged.display()))?;
            fs::rename(&staged, &path).with_context(|| format!("replace {}", path.display()))?;
        }
        Ok(())
    }

    /// Locks the builds directory, so no two updates run at once, and keeps
    /// it readable by the service's user whatever root's umask.
    fn lock(&self) -> Result<fs::File> {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            self.builds.is_dir(),
            "piqueld update updates hosts the install script set up, and {} doesn't exist; run the script instead",
            self.builds.display()
        );
        fs::set_permissions(&self.builds, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("open up {}", self.builds.display()))?;
        let path = self.builds.join(".lock");
        let lock = fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("another piqueld update is running")?;
        Ok(lock)
    }

    /// Installs the build `version`'s unit, if it differs, and reloads
    /// systemd.
    fn install_unit(&self, version: &str) -> Result<()> {
        let unit = self.build(version).join("piqueld.service");
        let wanted = fs::read(&unit).with_context(|| format!("read {}", unit.display()))?;
        if fs::read(&self.unit).ok().as_deref() != Some(wanted.as_slice()) {
            fs::write(&self.unit, wanted)
                .with_context(|| format!("write {}", self.unit.display()))?;
            systemctl(&["daemon-reload"])?;
        }
        Ok(())
    }

    /// Deletes every build but the current and previous ones, and what an
    /// interrupted update left.
    fn prune(&self) -> Result<()> {
        let keep = [self.linked("current"), self.linked("previous")];
        for entry in
            fs::read_dir(&self.builds).with_context(|| format!("list {}", self.builds.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale = name.starts_with('.') && entry.file_type()?.is_dir();
            let replaced = !name.starts_with('.')
                && entry.file_type()?.is_dir()
                && !keep.iter().flatten().any(|kept| *kept == name);
            if stale || replaced {
                fs::remove_dir_all(entry.path())
                    .with_context(|| format!("delete {}", entry.path().display()))?;
            }
        }
        Ok(())
    }
}

/// A published build to install: its version, and where its archive and
/// that archive's SHA-256 come from.
#[derive(Debug)]
pub struct Build {
    /// Its version, which names its directory.
    version: String,
    /// Where its archive comes from.
    archive: Archive,
    /// The archive's SHA-256, in hex.
    sha256: String,
}

/// Where a build's archive comes from.
#[derive(Debug)]
enum Archive {
    /// An HTTPS URL, such as its GitHub release's.
    Url(String),
    /// A file, for an offline install.
    File(PathBuf),
}

impl std::fmt::Display for Archive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(url) => formatter.write_str(url),
            Self::File(path) => path.display().fmt(formatter),
        }
    }
}

impl Build {
    /// `version` without a leading `v`, checked to name a build directory: a
    /// version's characters only, not starting with a dot, and not one of
    /// the links.
    fn version(version: &str) -> Result<&str> {
        let version = version.strip_prefix('v').unwrap_or(version);
        ensure!(
            version.starts_with(|c: char| c.is_ascii_digit())
                && version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
            "{version:?} isn't a piqueld version"
        );
        Ok(version)
    }

    /// The newest build of a channel, as its feed names it, whose archive
    /// must be served over HTTPS.
    fn from_feed(feed: piqueld_core::version::Feed) -> Result<Self> {
        ensure!(
            feed.archive_url.starts_with("https://"),
            "the feed names an archive outside HTTPS: {}",
            feed.archive_url
        );
        Ok(Self {
            version: Self::version(&feed.version.number)?.to_owned(),
            archive: Archive::Url(feed.archive_url),
            sha256: feed.archive_sha256,
        })
    }

    /// The build `version`, as its GitHub release publishes it, or as
    /// `from`, a directory holding the release's archive and `SHA256SUMS`,
    /// holds it.
    async fn published(version: &str, from: Option<&Path>, http: &reqwest::Client) -> Result<Self> {
        let version = Self::version(version)?;
        let name = format!("piqueld-{version}-x86_64-linux.tar.gz");
        let (archive, sums) = if let Some(from) = from {
            let sums = from.join("SHA256SUMS");
            let read =
                fs::read_to_string(&sums).with_context(|| format!("read {}", sums.display()))?;
            (Archive::File(from.join(&name)), read)
        } else {
            let release = format!("{REPOSITORY_URL}/releases/download/v{version}");
            let sums = format!("{release}/SHA256SUMS");
            let read = http
                .get(&sums)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("download {sums}"))?
                .text()
                .await
                .with_context(|| format!("download {sums}"))?;
            (Archive::Url(format!("{release}/{name}")), read)
        };
        let sha256 = sums
            .lines()
            .find_map(|line| line.strip_suffix(&format!("  {name}")))
            .with_context(|| format!("SHA256SUMS has no checksum for {name}"))?
            .to_owned();
        Ok(Self {
            version: version.to_owned(),
            archive,
            sha256,
        })
    }

    /// The archive's bytes, checked against its SHA-256.
    async fn fetch(&self, http: &reqwest::Client) -> Result<Vec<u8>> {
        let bytes = match &self.archive {
            Archive::Url(url) => http
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("download {url}"))?
                .bytes()
                .await
                .with_context(|| format!("download {url}"))?
                .to_vec(),
            Archive::File(path) => {
                fs::read(path).with_context(|| format!("read {}", path.display()))?
            }
        };
        let digest = format!("{:x}", Sha256::digest(&bytes));
        ensure!(
            digest == self.sha256,
            "{} doesn't match its SHA-256: expected {}, got {digest}",
            self.archive,
            self.sha256
        );
        Ok(bytes)
    }
}

/// Updates and rolls back the service of the host it runs on, as root.
pub struct Updater {
    /// Where the builds, links and unit are.
    layout: Layout,
    /// The daemon's configuration file, which a build must accept.
    config_path: PathBuf,
    /// That configuration, read without its credential files.
    config: DaemonConfig,
    /// Downloads archives and checksums, over HTTPS only.
    http: reqwest::Client,
}

impl Updater {
    /// Updates `layout`'s service, whose daemon reads `config_path`. It reads
    /// the configuration without its credential files, systemd credentials
    /// only the service can read, which the updater needs none of.
    ///
    /// # Errors
    /// Fails unless it runs as root, or when the configuration can't be read.
    pub fn new(layout: Layout, config_path: PathBuf) -> Result<Self> {
        ensure!(
            rustix::process::geteuid().is_root(),
            "piqueld update changes the host's piqueld: run it as root, such as with sudo"
        );
        let config = DaemonConfig::load_structural(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        let http = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .https_only(true)
            .user_agent(format!("piqueld/{}", piqueld_core::version::NUMBER))
            .build()
            .context("build the download client")?;
        Ok(Self {
            layout,
            config_path,
            config,
            http,
        })
    }

    /// A client of the daemon's socket, which root reaches as the host
    /// operator.
    fn daemon(&self) -> Result<piqueld_client::Client> {
        let socket = self
            .config
            .server
            .runtime_dir
            .join(piqueld_config::runtime_dir::SOCKET_NAME);
        piqueld_client::Client::unix(&socket)
            .map(|client| client.with_timeout(DAEMON_TIMEOUT))
            .with_context(|| format!("connect to {}", socket.display()))
    }

    /// The build to install: `version`, or the newest build of the channel
    /// the daemon follows, which it last checked.
    ///
    /// # Errors
    /// Fails when the daemon knows of no newer build, or can't be reached
    /// without `version`.
    pub async fn target(&self, version: Option<&str>, from: Option<&Path>) -> Result<Build> {
        if let Some(version) = version {
            return Build::published(version, from, &self.http).await;
        }
        let status = self.daemon()?.check_for_updates().await.context(
            "ask the daemon for its channel's newest build; to install one anyway, pass --version",
        )?;
        if let Some(error) = status.error {
            bail!("couldn't check the {} channel: {error}", status.channel);
        }
        match status.latest {
            Some(latest) if status.available => match from {
                Some(_) => Build::published(&latest.version.number, from, &self.http).await,
                None => Build::from_feed(latest),
            },
            _ => bail!(
                "piqueld {} is the newest build of the {} channel",
                status.current.number,
                status.channel
            ),
        }
    }

    /// Installs `build`, then restarts the daemon on it, going back to the
    /// previous build if it doesn't start.
    ///
    /// # Errors
    /// Fails, changing nothing, when the build can't be fetched or rejects
    /// the configuration, and when the daemon doesn't start on it.
    pub async fn update(&self, build: &Build) -> Result<()> {
        let layout = &self.layout;
        let _lock = layout.lock()?;
        let Some(current) = layout.linked("current") else {
            bail!(
                "piqueld update updates hosts the install script set up, and {} names no build; run the script instead",
                layout.builds.join("current").display()
            );
        };
        ensure!(
            current != build.version,
            "piqueld {} is already installed",
            build.version
        );
        self.unpack(build).await?;
        self.check_config(&build.version)?;
        tracing::info!("Starting piqueld {}", build.version);
        let started = self.switch(&build.version).await;
        if !started.healthy {
            return self
                .recover(&build.version, &current, started.since_ms)
                .await;
        }
        layout.link("previous", &current)?;
        layout.prune()?;
        tracing::info!("piqueld {} is running", build.version);
        Ok(())
    }

    /// Goes back to the previous build, unless the current one updated the
    /// database since it was installed: it did if it took the newest
    /// pre-migration backup, and an older build can't read that database.
    ///
    /// # Errors
    /// Fails when there is no previous build, when the database was
    /// updated, and when the previous build doesn't start.
    pub async fn rollback(&self) -> Result<()> {
        let layout = &self.layout;
        let _lock = layout.lock()?;
        let (Some(current), Some(previous)) = (layout.linked("current"), layout.linked("previous"))
        else {
            bail!("there is no previous build to roll back to");
        };
        ensure!(
            current != previous,
            "piqueld {current} is both the current and the previous build: there is nothing to roll back to"
        );
        let running = layout.reported_version(&current)?;
        if let Some((backup, migrated_by)) =
            Backups::new(&self.config.server.data_dir).latest_migration()?
            && migrated_by == running
        {
            bail!(
                "rollback only returns to builds older than piqueld {current}, which updated the database: to go to a newer build, run piqueld update --version <version>; to go back to piqueld {previous}, restore {}, which discards every change since, see https://docs.piqueld.dev/user/operations/backups.html#roll-back",
                backup.display()
            );
        }
        self.check_config(&previous)?;
        tracing::info!("Starting piqueld {previous}");
        let started = self.switch(&previous).await;
        if !started.healthy {
            return self.recover(&previous, &current, started.since_ms).await;
        }
        layout.link("previous", &current)?;
        tracing::info!("piqueld {previous} is running");
        Ok(())
    }

    /// Downloads, checks and unpacks `build` into its directory, unless it
    /// is already there, such as after a rollback. Whatever root's umask and
    /// the archive's modes, the service's user can run its binaries and read
    /// the rest.
    async fn unpack(&self, build: &Build) -> Result<()> {
        let directory = self.layout.build(&build.version);
        if directory.join("piqueld").exists() {
            tracing::info!(
                "Using piqueld {}, already unpacked in {}",
                build.version,
                directory.display()
            );
            return Ok(());
        }
        tracing::info!(
            "Downloading piqueld {} from {}",
            build.version,
            build.archive
        );
        let archive = build.fetch(&self.http).await?;
        let staging = self
            .layout
            .builds
            .join(format!(".{}.partial", build.version));
        remove(&staging)?;
        fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
        let file = staging.join("archive.tar.gz");
        fs::write(&file, archive).with_context(|| format!("write {}", file.display()))?;
        let files = staging.join("files");
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o755)
            .create(&files)
            .with_context(|| format!("create {}", files.display()))?;
        fs::set_permissions(&files, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .with_context(|| format!("open up {}", files.display()))?;
        run(Command::new("tar")
            .args([
                "--extract",
                "--gzip",
                "--strip-components=1",
                "--no-same-owner",
            ])
            .arg("--file")
            .arg(&file)
            .arg("--directory")
            .arg(&files))?;
        ensure!(
            files.join("piqueld").exists() && files.join("piqueld.service").exists(),
            "{} isn't a piqueld release archive",
            build.archive
        );
        for entry in fs::read_dir(&files).with_context(|| format!("list {}", files.display()))? {
            let path = entry?.path();
            let executable = path.is_dir()
                || matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("piqueld" | "piquelctl")
                );
            let mode = if executable { 0o755 } else { 0o644 };
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode))
                .with_context(|| format!("open up {}", path.display()))?;
        }
        remove(&directory)?;
        fs::rename(&files, &directory)
            .with_context(|| format!("move the build to {}", directory.display()))?;
        fs::remove_dir_all(&staging).with_context(|| format!("delete {}", staging.display()))
    }

    /// Runs the build `version`'s `piqueld check-config` on the daemon's
    /// configuration.
    fn check_config(&self, version: &str) -> Result<()> {
        let daemon = self.layout.build(version).join("piqueld");
        run(Command::new(&daemon)
            .arg("check-config")
            .arg("--config")
            .arg(&self.config_path))
        .with_context(|| format!("piqueld {version} rejects {}", self.config_path.display()))
    }

    /// Points `current` at `version`, then restarts the service on it and
    /// waits for the daemon to answer. A step that fails counts as a daemon
    /// that didn't start, so the caller goes back.
    async fn switch(&self, version: &str) -> Started {
        let layout = &self.layout;
        let since_ms = piqueld_store::now_ms();
        let switched = layout
            .install_unit(version)
            .and_then(|()| layout.link("current", version))
            .and_then(|()| layout.link_binaries())
            .and_then(|()| systemctl(&["restart", SERVICE]));
        let healthy = match switched {
            Ok(()) => self.answers().await,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "couldn't start piqueld {version}"
                );
                false
            }
        };
        Started { healthy, since_ms }
    }

    /// Whether the daemon answers on its socket within [`START_TIMEOUT`].
    async fn answers(&self) -> bool {
        let Ok(daemon) = self.daemon() else {
            return false;
        };
        piqueld_core::wait::poll_until(START_TIMEOUT, START_POLL, || async {
            daemon.system_status().await.ok()
        })
        .await
        .is_some()
    }

    /// Goes back from `failed`, which didn't start, to `previous`, unless
    /// `failed` updated the database, then fails either way. When it did, it
    /// links `previous`, so `rollback` returns to it once the backup is
    /// restored.
    async fn recover(&self, failed: &str, previous: &str, since_ms: i64) -> Result<()> {
        if let Some(backup) = self.migrated_since(since_ms)? {
            if let Err(error) = self.layout.link("previous", previous) {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "couldn't link piqueld {previous} as the previous build"
                );
            }
            bail!(
                "piqueld {failed} didn't start after updating the database, so piqueld {previous} can't run on it; see: journalctl -u piqueld. To go back, restore {}: https://docs.piqueld.dev/user/operations/backups.html#roll-back",
                backup.display()
            );
        }
        tracing::warn!("piqueld {failed} didn't start; going back to piqueld {previous}");
        let started = self.switch(previous).await;
        ensure!(
            started.healthy,
            "neither piqueld {failed} nor piqueld {previous} started; see: journalctl -u piqueld"
        );
        bail!(
            "piqueld {failed} didn't start, so piqueld {previous} runs again; see: journalctl -u piqueld"
        )
    }

    /// The backup the daemon took before updating the database, if it did
    /// after `since_ms`.
    fn migrated_since(&self, since_ms: i64) -> Result<Option<PathBuf>> {
        Ok(Backups::new(&self.config.server.data_dir)
            .latest_before_migration()?
            .filter(|(_, backup)| backup.created_at_ms >= since_ms)
            .map(|(path, _)| path))
    }
}

/// How a restart went: whether the daemon answered, and when it began.
struct Started {
    /// Whether the daemon answered within [`START_TIMEOUT`].
    healthy: bool,
    /// When the restart began, in Unix milliseconds.
    since_ms: i64,
}

/// Runs `systemctl` with `args`.
fn systemctl(args: &[&str]) -> Result<()> {
    run(Command::new("systemctl").args(args))
}

/// Runs `command`, failing with its output unless it succeeds.
fn run(command: &mut Command) -> Result<()> {
    let name = command.get_program().to_string_lossy().into_owned();
    let output = command.output().with_context(|| format!("run {name}"))?;
    ensure!(
        output.status.success(),
        "{name} failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests;

/// Removes the file, link or directory at `path`, if there is one. A link is
/// removed, never followed.
fn remove(path: &Path) -> Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    };
    removed.with_context(|| format!("delete {}", path.display()))
}
