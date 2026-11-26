//! Whether this installation's release channel has a newer build.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::names::{compared_as_string, string_enum};
use crate::version::{Channel, Feed, Version};

string_enum! {
    /// How piqueld was installed, which decides how to update it.
    pub enum InstallMethod {
        /// From a Nix package, such as the NixOS module's: update the flake
        /// input, then rebuild.
        Nix => "nix",
        /// Where piqueld's updater installs builds: hosts the install script
        /// set up, and NixOS's managed mode. It updates from piqueld.
        Script => "script",
        /// Some other way, such as `cargo install`.
        Other => "other",
    }
}
compared_as_string!(InstallMethod);

/// The channel an installation follows, and whether it has a newer build
/// than the running daemon. piqueld only checks: updating is the operator's
/// choice, by [`InstallMethod`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct UpdateStatus {
    /// The running daemon's version.
    pub current: Version,
    /// The [`Channel`] it follows.
    pub channel: String,
    /// Whether `updates.channel` in the daemon configuration sets the channel,
    /// so only the configuration changes it.
    pub channel_configured: bool,
    /// Whether it checks its channel for newer builds: `updates.check`.
    pub checks: bool,
    /// The [`InstallMethod`], which decides how to update.
    pub install: String,
    /// The channel's newest build, as of the last check that reached its
    /// feed; absent before one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<Feed>,
    /// Whether `latest` is newer than the running daemon.
    pub available: bool,
    /// When the channel was last checked, in Unix milliseconds; absent before
    /// the first check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<i64>,
    /// Why the last check failed; absent when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The build `piqueld update rollback` returns to, on a host the install
    /// script set up or with NixOS's managed updates, where it is `package`,
    /// the module's package, after the first update; absent elsewhere, and
    /// before an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The latest update or rollback on this host, as the updater records
    /// it; absent before one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<UpdateRun>,
}

/// The [`UpdateStatus::previous`] of NixOS's managed updates after the
/// first one: the module's package, which a rollback returns to. No version
/// starts with a letter, so no build has this name.
pub const PACKAGE_BUILD: &str = "package";

string_enum! {
    #[serde]
    /// What a run of the updater does.
    pub enum UpdateAction {
        /// Installs a newer build, then restarts piqueld on it.
        Update => "update",
        /// Returns to the build the last update replaced.
        Rollback => "rollback",
    }
}
compared_as_string!(UpdateAction);

string_enum! {
    #[serde]
    /// Where a run of the updater is.
    pub enum UpdateRunState {
        /// The daemon asked for it, and the updater hasn't started yet.
        Requested => "requested",
        /// The updater is installing the build or restarting piqueld.
        Running => "running",
        /// piqueld runs on the build.
        Succeeded => "succeeded",
        /// The run failed; its message says why, and what runs now.
        Failed => "failed",
    }
}
compared_as_string!(UpdateRunState);

/// One run of the updater: an update or a rollback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct UpdateRun {
    /// The [`UpdateAction`].
    pub action: String,
    /// The build it installs, or returns to; absent while the updater
    /// resolves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The [`UpdateRunState`].
    pub state: String,
    /// What happened, such as why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When it was requested or started, in Unix milliseconds.
    pub started_at_ms: i64,
    /// When it finished, in Unix milliseconds; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
}

impl UpdateRun {
    /// Whether it hasn't finished: requested or running.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.state == UpdateRunState::Requested || self.state == UpdateRunState::Running
    }
}

/// What the daemon asks the updater to do, through the request file the
/// `piqueld-update.path` unit watches. An update names the version, which
/// the updater downloads from its release itself, so the daemon chooses
/// neither the archive nor its checksum.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UpdateRequest {
    /// Install this published version, newer than the running one.
    Update {
        /// Its version.
        version: String,
    },
    /// Return to the build the last update replaced.
    Rollback,
}

impl UpdateStatus {
    /// How to install the newer build, for people, by [`InstallMethod`];
    /// absent while none is available. It names the channel's tag, which a
    /// flake input must follow to get its builds.
    #[must_use]
    pub fn how_to_update(&self) -> Option<String> {
        let latest = self.latest.as_ref().filter(|_| self.available)?;
        let number = &latest.version.number;
        let install = InstallMethod::parse(&self.install).unwrap_or(InstallMethod::Other);
        Some(match install {
            InstallMethod::Nix => format!(
                "Point the piqueld flake input at github:sourceorigin-dev/piqueld/{}, then: nix flake update piqueld && sudo nixos-rebuild switch",
                self.channel
            ),
            InstallMethod::Script => {
                "Install it with piquelctl update apply or the dashboard, or on the host: sudo piqueld update".to_owned()
            }
            InstallMethod::Other => {
                format!("Install piqueld {number} the way you installed this build.")
            }
        })
    }
}

/// Follows another release channel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct UpdateChannelRequest {
    /// The channel to follow.
    pub channel: Channel,
}
