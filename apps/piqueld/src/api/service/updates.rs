//! Checks for newer piqueld builds on the release channel the installation
//! follows. piqueld only checks and notifies; the operator updates it, the
//! way it was installed.

use super::{ApplicationError, ApplicationService, Author};
use anyhow::Context;
use piqueld_config::UpdatesConfig;
use piqueld_core::api::{
    InstallMethod, UpdateAction, UpdateRequest, UpdateRun, UpdateRunState, UpdateStatus,
};
use piqueld_core::version::{Channel, Feed, Version};
use piqueld_store::now_ms;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Time between checks of the channel's feed.
const CHECK_INTERVAL: Duration = Duration::from_hours(6);
/// Time before checking again after a check failed, such as at boot before
/// the network is up; it doubles with each failure in a row, up to
/// [`CHECK_INTERVAL`].
const RETRY_INTERVAL: Duration = Duration::from_mins(10);
/// Time after which a run the updater hasn't finished no longer blocks
/// another, such as one whose request it never took.
const RUN_TIMEOUT: Duration = Duration::from_mins(30);
/// The updater's path unit, which takes the daemon's requests, as the
/// install script and the updater install it.
const UPDATE_PATH_UNIT: &str = "/etc/systemd/system/piqueld-update.path";
/// The runtime directory whose request the path unit watches.
const UPDATE_RUNTIME_DIR: &str = "/run/piqueld";
/// Time a check may take, redirects included.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The release channel's feed, and what its last check found.
pub(super) struct Updates {
    /// The daemon configuration's `[updates]`.
    config: UpdatesConfig,
    /// Fetches the feeds, within [`CHECK_TIMEOUT`].
    client: reqwest::Client,
    /// How this binary was installed.
    install: InstallMethod,
    /// What the last check found, which the status reports.
    last: Mutex<LastCheck>,
    /// Counts channel changes, so a check that started before one doesn't
    /// replace the check of the new channel.
    changes: AtomicU64,
    /// Where the updater takes requests and records its runs; absent without
    /// a configuration, as in tests.
    directories: Option<Directories>,
}

/// The daemon's directories the updater shares (see `crate::updater`).
#[derive(Clone)]
struct Directories {
    /// The data directory, holding the updater's latest run.
    data: PathBuf,
    /// The runtime directory, where the daemon writes its requests.
    runtime: PathBuf,
}

/// What the last check of a channel found.
#[derive(Clone, Default)]
struct LastCheck {
    /// The channel it checked; a check of another channel tells nothing.
    channel: Option<Channel>,
    /// The channel's newest build, from the last check that reached its feed.
    latest: Option<Feed>,
    /// When it last checked, in Unix milliseconds.
    checked_at_ms: Option<i64>,
    /// Why the last check couldn't read the feed, if it couldn't.
    error: Option<String>,
}

impl Updates {
    /// Never checks on its own, until [`Self::configured`], as in tests.
    pub(super) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            config: UpdatesConfig {
                check: false,
                ..UpdatesConfig::default()
            },
            client: reqwest::Client::builder()
                .timeout(CHECK_TIMEOUT)
                .user_agent(format!("piqueld/{}", piqueld_core::version::NUMBER))
                .build()
                .context("build the update check client")?,
            install: Self::install_method(),
            last: Mutex::default(),
            changes: AtomicU64::new(0),
            directories: None,
        })
    }

    /// Checks as `config` says, with the same client, and shares its
    /// directories with the updater.
    pub(super) fn configured(&self, config: &piqueld_config::DaemonConfig) -> Self {
        Self {
            config: config.updates.clone(),
            client: self.client.clone(),
            install: self.install,
            last: Mutex::default(),
            changes: AtomicU64::new(0),
            directories: Some(Directories {
                data: config.server.data_dir.clone(),
                runtime: config.server.runtime_dir.clone(),
            }),
        }
    }

    /// The updater's latest run, from [`crate::updater::RUN_FILE`]. One the
    /// updater never finished, such as on a reboot, has failed after
    /// [`RUN_TIMEOUT`].
    fn run(&self) -> Option<UpdateRun> {
        let path = self
            .directories
            .as_ref()?
            .data
            .join(crate::updater::RUN_FILE);
        let bytes = std::fs::read(&path).ok()?;
        let mut run: UpdateRun = serde_json::from_slice(&bytes)
            .inspect_err(
                |error| tracing::warn!(%error, path = %path.display(), "unreadable update run"),
            )
            .ok()?;
        let timeout = i64::try_from(RUN_TIMEOUT.as_millis()).unwrap_or(i64::MAX);
        if run.in_progress() && now_ms() - run.started_at_ms > timeout {
            run.state = UpdateRunState::Failed.into();
            run.message = Some(
                "the updater didn't finish within 30 minutes; see: journalctl -u piqueld-update"
                    .into(),
            );
        }
        Some(run)
    }

    /// The build `piqueld update rollback` returns to, on a host the install
    /// script set up or with NixOS's managed updates.
    fn previous(&self) -> Option<String> {
        (self.install == InstallMethod::Script)
            .then(|| crate::updater::Layout::detect().linked("previous"))
            .flatten()
    }

    /// How this binary was installed, from where it runs: the Nix store, a
    /// build directory of the install script's (`crate::updater::Layout`),
    /// or a binary an older script copied to `/usr/local/bin`, or elsewhere.
    /// NixOS's managed mode updates as the script's does, though its
    /// module's package, in the Nix store, runs before the first update.
    fn install_method() -> InstallMethod {
        if crate::updater::Layout::detect().is_managed() {
            return InstallMethod::Script;
        }
        match std::env::current_exe() {
            Ok(path) if path.starts_with("/nix/store") => InstallMethod::Nix,
            Ok(path)
                if path.starts_with("/usr/local/lib/piqueld")
                    || path == Path::new("/usr/local/bin/piqueld") =>
            {
                InstallMethod::Script
            }
            _ => InstallMethod::Other,
        }
    }

    /// The last check, as of now.
    fn last(&self) -> LastCheck {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Fetches `channel`'s feed.
    async fn fetch(&self, channel: Channel) -> anyhow::Result<Feed> {
        let url = format!(
            "{}/{}",
            self.config.feed_url.trim_end_matches('/'),
            channel.feed_file()
        );
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("couldn't reach {url}"))?;
        anyhow::ensure!(
            response.status() != reqwest::StatusCode::NOT_FOUND,
            "the {channel} channel has no published build at {url}"
        );
        response
            .error_for_status()
            .with_context(|| format!("couldn't read {url}"))?
            .json()
            .await
            .with_context(|| format!("{url} isn't a channel feed"))
    }
}

impl ApplicationService {
    /// The channel this installation follows: the configuration's, else the
    /// one operators chose, else this build's, else stable.
    async fn update_channel(&self) -> Result<Channel, ApplicationError> {
        if let Some(channel) = self.updates.config.channel {
            return Ok(channel);
        }
        Ok(self.store.update_channel().await?.unwrap_or_else(|| {
            Version::current()
                .channel
                .as_deref()
                .and_then(Channel::parse)
                .unwrap_or(Channel::Stable)
        }))
    }

    /// The channel this installation follows, and whether, as of the last
    /// check, it has a newer build than this daemon.
    /// # Errors
    /// Returns storage errors.
    pub async fn update_status(&self) -> Result<UpdateStatus, ApplicationError> {
        let channel = self.update_channel().await?;
        let current = Version::current();
        let last = self.updates.last();
        let last = if last.channel == Some(channel) {
            last
        } else {
            LastCheck::default()
        };
        Ok(UpdateStatus {
            available: last
                .latest
                .as_ref()
                .is_some_and(|latest| latest.version.is_newer_than(&current)),
            current,
            channel: channel.into(),
            channel_configured: self.updates.config.channel.is_some(),
            checks: self.updates.config.check,
            install: self.updates.install.into(),
            latest: last.latest,
            checked_at_ms: last.checked_at_ms,
            error: last.error,
            previous: self.updates.previous(),
            run: self.updates.run(),
        })
    }

    /// Checks the channel's feed now. The first time it finds a newer build
    /// than this daemon, it records an `update_available` event, which
    /// notifies. A feed it can't read is the status's `error`. A check the
    /// channel changed during tells nothing, so it returns the status
    /// without recording it.
    /// # Errors
    /// Returns storage errors.
    pub async fn check_for_updates(&self) -> Result<UpdateStatus, ApplicationError> {
        let changes = self.updates.changes.load(Ordering::SeqCst);
        let channel = self.update_channel().await?;
        let fetched = self.updates.fetch(channel).await;
        {
            let mut last = self
                .updates
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.updates.changes.load(Ordering::SeqCst) != changes {
                drop(last);
                return self.update_status().await;
            }
            if last.channel != Some(channel) {
                *last = LastCheck::default();
            }
            last.channel = Some(channel);
            last.checked_at_ms = Some(now_ms());
            match &fetched {
                Ok(feed) => {
                    last.latest = Some(feed.clone());
                    last.error = None;
                }
                Err(error) => last.error = Some(format!("{error:#}")),
            }
        }
        if let Ok(feed) = fetched
            && feed.version.is_newer_than(&Version::current())
        {
            let number = &feed.version.number;
            let message = format!(
                "piqueld {number} is available on the {channel} channel (running {})",
                piqueld_core::version::NUMBER
            );
            self.store.record_update_available(number, &message).await?;
        }
        self.update_status().await
    }

    /// Follows `channel`, unless the configuration sets it, then checks it.
    /// It counts the change before the write, so no check of the old channel
    /// lands after it.
    /// # Errors
    /// Returns [`ApplicationError::UpdateChannelConfigured`], refusal, or
    /// storage errors.
    pub async fn set_update_channel(
        &self,
        author: Author<'_>,
        channel: Channel,
    ) -> Result<UpdateStatus, ApplicationError> {
        if self.updates.config.channel.is_some() {
            return Err(ApplicationError::UpdateChannelConfigured);
        }
        self.updates.changes.fetch_add(1, Ordering::SeqCst);
        self.store.set_update_channel(author, channel).await?;
        self.check_for_updates().await
    }

    /// Asks the updater, which runs as root, to install the channel's newer
    /// build, or roll back to the previous one, then restart piqueld; the
    /// status's `run` follows it. The path unit watches only
    /// [`UPDATE_RUNTIME_DIR`]'s request, and the updater refuses an older
    /// number, which a later commit can have, as a release after a nightly.
    /// # Errors
    /// Returns [`ApplicationError::UpdateUnavailable`] when this host can't
    /// update from piqueld, or has no build to go to,
    /// [`ApplicationError::UpdateRunning`] while a run hasn't finished, and
    /// storage or file errors.
    pub async fn request_update(
        &self,
        action: UpdateAction,
    ) -> Result<UpdateStatus, ApplicationError> {
        let status = self.update_status().await?;
        let unavailable = |why: String| Err(ApplicationError::UpdateUnavailable(why));
        let Some(directories) = &self.updates.directories else {
            return unavailable("this daemon runs without a configuration".into());
        };
        if self.updates.install != InstallMethod::Script {
            let how = status
                .how_to_update()
                .unwrap_or_else(|| "Update it the way you installed it.".into());
            return unavailable(format!(
                "piqueld updates itself only where the install script set it up, or with NixOS's managed updates; {how}"
            ));
        }
        if !Path::new(UPDATE_PATH_UNIT).exists() {
            return unavailable(
                "this host lacks the units that apply updates: run the install script again, or update on the host with sudo piqueld update".into(),
            );
        }
        if directories.runtime != Path::new(UPDATE_RUNTIME_DIR) {
            return unavailable(format!(
                "the update units take requests in {UPDATE_RUNTIME_DIR}, but server.runtime_dir is {}; update on the host with sudo piqueld update",
                directories.runtime.display()
            ));
        }
        if status.run.as_ref().is_some_and(UpdateRun::in_progress) {
            return Err(ApplicationError::UpdateRunning);
        }
        let version = match action {
            UpdateAction::Update => match status.latest.as_ref().filter(|_| status.available) {
                Some(latest)
                    if !Version::numbered(&latest.version.number)
                        .is_newer_than(&Version::numbered(&status.current.number)) =>
                {
                    let hint = if latest.version.number == status.current.number {
                        String::new()
                    } else {
                        format!(
                            ": install it on the host with sudo piqueld update --version {}",
                            latest.version.number
                        )
                    };
                    return unavailable(format!(
                        "piqueld {} isn't a newer version than {}{hint}",
                        latest.version.number, status.current.number
                    ));
                }
                Some(latest) => latest.version.number.clone(),
                None => {
                    return unavailable(format!(
                        "the {} channel has no newer build than {}",
                        status.channel, status.current.number
                    ));
                }
            },
            UpdateAction::Rollback => match &status.previous {
                Some(previous) => previous.clone(),
                None => return unavailable("there is no previous build to roll back to".into()),
            },
        };
        let mut run = UpdateRun {
            action: action.into(),
            version: Some(version.clone()),
            state: UpdateRunState::Requested.into(),
            message: None,
            started_at_ms: now_ms(),
            finished_at_ms: None,
        };
        let request = match action {
            UpdateAction::Update => UpdateRequest::Update { version },
            UpdateAction::Rollback => UpdateRequest::Rollback,
        };
        let run_file = directories.data.join(crate::updater::RUN_FILE);
        Self::write_atomically(&run_file, &run)?;
        let requested = Self::write_atomically(
            &directories.runtime.join(crate::updater::REQUEST_FILE),
            &request,
        );
        if let Err(error) = requested {
            run.state = UpdateRunState::Failed.into();
            run.finished_at_ms = Some(now_ms());
            run.message = Some("the daemon couldn't ask the updater; see its log".into());
            Self::write_atomically(&run_file, &run)?;
            return Err(error);
        }
        self.update_status().await
    }

    /// Replaces `path` with `value` as JSON, so a reader never sees half of
    /// it.
    fn write_atomically(
        path: &Path,
        value: &impl serde::Serialize,
    ) -> Result<(), ApplicationError> {
        let staged = path.with_extension("new");
        serde_json::to_vec(value)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                std::fs::write(&staged, bytes)?;
                std::fs::rename(&staged, path)?;
                Ok(())
            })
            .with_context(|| format!("write {}", path.display()))
            .map_err(ApplicationError::UpdateFile)
    }

    /// Checks the channel at once, then every [`CHECK_INTERVAL`], or sooner
    /// after a failed check (see [`RETRY_INTERVAL`]), until cancelled, unless
    /// `updates.check` is off.
    pub(super) async fn watch_updates(&self, cancellation: CancellationToken) {
        if !self.updates.config.check {
            return;
        }
        let mut retry = RETRY_INTERVAL;
        loop {
            let checked = tokio::select! {
                () = cancellation.cancelled() => return,
                checked = self.check_for_updates() => checked,
            };
            let failed = match checked {
                Ok(UpdateStatus {
                    error: Some(error), ..
                }) => {
                    tracing::warn!(%error, "couldn't check for updates");
                    true
                }
                Ok(_) => false,
                Err(error) => {
                    tracing::error!(error = %error, "couldn't record an update check");
                    true
                }
            };
            let wait = if failed {
                let wait = retry;
                retry = (retry * 2).min(CHECK_INTERVAL);
                wait
            } else {
                retry = RETRY_INTERVAL;
                CHECK_INTERVAL
            };
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = piqueld_core::wait::pause(wait) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_store::Store;
    use std::future::IntoFuture;
    use std::sync::Arc;

    /// A check finds the channel's newer build, and records it as available
    /// once, however often it checks.
    #[tokio::test]
    async fn a_newer_build_on_the_channel_is_available_and_recorded_once() {
        let feed = Feed {
            version: Version {
                number: "9.0.0-nightly.20991231".into(),
                commit: Some("f".repeat(40)),
                committed_at_ms: Some(i64::MAX),
                channel: Some("nightly".into()),
            },
            notes_url: "https://example.com/notes".into(),
            archive_url: "https://example.com/archive.tar.gz".into(),
            archive_sha256: "0".repeat(64),
        };
        let published = feed.clone();
        let feeds = axum::Router::new().route(
            "/nightly.json",
            axum::routing::get(move || std::future::ready(axum::Json(published.clone()))),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the feed server binds");
        let address = listener
            .local_addr()
            .expect("the feed server has an address");
        let server = tokio::spawn(axum::serve(listener, feeds).into_future());
        let temp = tempfile::tempdir().expect("a temporary directory");
        let store = Arc::new(
            Store::open_unsynced(temp.path().join("db"))
                .await
                .expect("the store opens"),
        );
        let mut config = piqueld_config::DaemonConfig::default();
        config.updates.channel = Some(Channel::Nightly);
        config.updates.feed_url = format!("http://{address}");
        let service = ApplicationService::unconnected(store.clone(), temp.path())
            .await
            .with_configuration(&config);

        let status = service
            .check_for_updates()
            .await
            .expect("the first check succeeds");
        service
            .check_for_updates()
            .await
            .expect("the second check succeeds");
        server.abort();

        assert!(status.available, "{status:?}");
        assert_eq!(status.channel, Channel::Nightly);
        assert_eq!(status.latest, Some(feed));
        assert_eq!(status.error, None);
        let again = store
            .record_update_available("9.0.0-nightly.20991231", "again")
            .await
            .expect("the update is recorded");
        assert!(!again, "both checks recorded the update once");
    }

    /// Only a host the install script set up updates from piqueld; another
    /// is told how to update instead, and no request is written.
    #[tokio::test]
    async fn hosts_the_script_didnt_set_up_are_told_how_to_update() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let store = Arc::new(
            Store::open_unsynced(temp.path().join("db"))
                .await
                .expect("the store opens"),
        );
        let mut config = piqueld_config::DaemonConfig::default();
        config.server.runtime_dir = temp.path().join("run");
        std::fs::create_dir(&config.server.runtime_dir).expect("the runtime directory is created");
        let service = ApplicationService::unconnected(store, temp.path())
            .await
            .with_configuration(&config);

        let refused = service.request_update(UpdateAction::Update).await;

        assert!(
            matches!(&refused, Err(ApplicationError::UpdateUnavailable(why)) if why.contains("install script")),
            "{refused:?}"
        );
        assert!(
            !config
                .server
                .runtime_dir
                .join(crate::updater::REQUEST_FILE)
                .exists()
        );
    }
}
