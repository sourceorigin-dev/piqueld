//! Checks for newer piqueld builds on the release channel the installation
//! follows. piqueld only checks and notifies; the operator updates it, the
//! way it was installed.

use super::{ApplicationError, ApplicationService, Author};
use anyhow::Context;
use piqueld_config::UpdatesConfig;
use piqueld_core::api::{InstallMethod, UpdateStatus};
use piqueld_core::version::{Channel, Feed, Version};
use piqueld_store::now_ms;
use std::path::Path;
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
        })
    }

    /// Checks as `config` says, with the same client.
    pub(super) fn configured(&self, config: UpdatesConfig) -> Self {
        Self {
            config,
            client: self.client.clone(),
            install: self.install,
            last: Mutex::default(),
            changes: AtomicU64::new(0),
        }
    }

    /// How this binary was installed, from where it runs: the Nix store, the
    /// install script's `/usr/local/bin`, or elsewhere.
    fn install_method() -> InstallMethod {
        match std::env::current_exe() {
            Ok(path) if path.starts_with("/nix/store") => InstallMethod::Nix,
            Ok(path) if path == Path::new("/usr/local/bin/piqueld") => InstallMethod::Script,
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
}
