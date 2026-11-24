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
        /// By the install script: update with `sudo piqueld update`.
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
                "Install it on the host, which restarts piqueld: sudo piqueld update".to_owned()
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
