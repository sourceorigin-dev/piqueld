//! What a piqueld binary is: its version, and the commit and channel a
//! release build was made from.
//!
//! Release builds stamp these at compile time through environment
//! variables: `cargo xtask dist` and the Nix packages set
//! `PIQUELD_BUILD_COMMIT` and `PIQUELD_BUILD_COMMIT_TIME`, `cargo xtask dist`
//! sets `PIQUELD_BUILD_VERSION`, which replaces the crate version, and with
//! `--channel`, `PIQUELD_BUILD_CHANNEL`. Other builds, such as `cargo install` and
//! development builds, know only their crate version. A commit time or
//! channel that doesn't parse fails the build.

use crate::names::{compared_as_string, string_enum};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

string_enum! {
    #[serde]
    /// A stream of piqueld versions an installation can follow.
    pub enum Channel {
        /// The versions a maintainer releases.
        Stable => "stable",
        /// A build of the latest `main` every night, once it passes the
        /// release checks.
        Nightly => "nightly",
    }
}

/// A piqueld binary's version, and what it was built from when a release
/// build stamped it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Version {
    /// Its version, such as `0.3.0`, or `0.4.0-nightly.20261125` for a
    /// nightly.
    pub number: String,
    /// The Git commit it was built from; absent from builds that weren't
    /// stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// When that commit was made, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at_ms: Option<i64>,
    /// The channel it was published on, `stable` or `nightly`; absent from
    /// builds that weren't published on one, such as Nix packages. A string,
    /// not a [`Channel`], so a client still reads one a newer daemon adds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

compared_as_string!(Channel);

/// The repository whose GitHub releases hold every published build, and the
/// `channels` release with each channel's [`Feed`].
pub const REPOSITORY_URL: &str = "https://github.com/sourceorigin-dev/piqueld";

impl std::str::FromStr for Channel {
    type Err = UnknownChannel;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::parse(name).ok_or_else(|| UnknownChannel(name.to_owned()))
    }
}

/// A name that isn't a [`Channel`]'s.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0} isn't a channel: stable or nightly")]
pub struct UnknownChannel(pub String);

impl Channel {
    /// The file its feed is published as, such as `nightly.json`.
    #[must_use]
    pub fn feed_file(self) -> String {
        format!("{self}.json")
    }
}

/// The newest build on a channel. The workflow that publishes a build on a
/// channel also replaces its feed, so installations can tell whether their
/// channel has a newer build than theirs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Feed {
    /// The build: its version, commit and channel.
    pub version: Version,
    /// Its release page, with its notes.
    pub notes_url: String,
    /// Its `x86_64` Linux release archive, which install.sh installs.
    pub archive_url: String,
    /// The archive's SHA-256, in lowercase hex.
    pub archive_sha256: String,
}

/// This binary's version number, for `--version`.
pub const NUMBER: &str = match option_env!("PIQUELD_BUILD_VERSION") {
    Some(number) => number,
    None => env!("CARGO_PKG_VERSION"),
};
/// The commit a release build stamped, `PIQUELD_BUILD_COMMIT`.
const COMMIT: Option<&str> = option_env!("PIQUELD_BUILD_COMMIT");
/// When [`COMMIT`] was made, in Unix milliseconds, from
/// `PIQUELD_BUILD_COMMIT_TIME`; a value that doesn't parse fails the build.
const COMMITTED_AT_MS: Option<i64> = match option_env!("PIQUELD_BUILD_COMMIT_TIME") {
    Some(seconds) => Some(parse_seconds(seconds) * 1000),
    None => None,
};
/// The channel a release build stamped, `PIQUELD_BUILD_CHANNEL`; a name
/// that isn't a [`Channel`]'s fails the build.
const CHANNEL: Option<Channel> = match option_env!("PIQUELD_BUILD_CHANNEL") {
    Some(text) => {
        let channel = parse_channel(text);
        assert!(
            channel.is_some(),
            "PIQUELD_BUILD_CHANNEL must be a channel's name"
        );
        channel
    }
    None => None,
};

impl Version {
    /// This binary's version.
    #[must_use]
    pub fn current() -> Self {
        Self {
            number: NUMBER.to_owned(),
            commit: COMMIT.map(str::to_owned),
            committed_at_ms: COMMITTED_AT_MS,
            channel: CHANNEL.map(String::from),
        }
    }

    /// A version known only by its number, which compares by it.
    #[must_use]
    pub fn numbered(number: &str) -> Self {
        Self {
            number: number.to_owned(),
            commit: None,
            committed_at_ms: None,
            channel: None,
        }
    }

    /// Whether `self` is a newer build than `other`: made from a later
    /// commit, or, when either doesn't know its commit's time, a higher
    /// version.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        match (self.committed_at_ms, other.committed_at_ms) {
            (Some(this), Some(other)) => this > other,
            _ => match (Self::order(&self.number), Self::order(&other.number)) {
                (Some(this), Some(other)) => this > other,
                _ => false,
            },
        }
    }

    /// How `number`, `major.minor.patch` with an optional `-pre-release`,
    /// sorts: by its numbers, then a release after its pre-releases, which
    /// sort by name, as dated nightlies do.
    fn order(number: &str) -> Option<(u64, u64, u64, bool, &str)> {
        let (release, pre) = number.split_once('-').unwrap_or((number, ""));
        let mut parts = release.splitn(3, '.').map(str::parse);
        let (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) =
            (parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Some((major, minor, patch, pre.is_empty(), pre))
    }

    /// The commit, shortened for display as [`crate::sync::short_commit`]
    /// shortens environments' commits.
    #[must_use]
    pub fn short_commit(&self) -> Option<&str> {
        self.commit.as_deref().map(crate::sync::short_commit)
    }
}

/// Parses `PIQUELD_BUILD_COMMIT_TIME`, Unix seconds as `git log --format=%ct`
/// prints them.
const fn parse_seconds(text: &str) -> i64 {
    let bytes = text.as_bytes();
    assert!(
        !bytes.is_empty() && bytes.len() <= 12,
        "PIQUELD_BUILD_COMMIT_TIME must be Unix seconds"
    );
    let mut seconds = 0;
    let mut index = 0;
    while index < bytes.len() {
        let digit = bytes[index];
        assert!(
            digit.is_ascii_digit(),
            "PIQUELD_BUILD_COMMIT_TIME must be Unix seconds"
        );
        seconds = seconds * 10 + (digit - b'0') as i64;
        index += 1;
    }
    seconds
}

/// Parses `PIQUELD_BUILD_CHANNEL`, a [`Channel`]'s string form, or `None`
/// for any other text.
const fn parse_channel(text: &str) -> Option<Channel> {
    let mut index = 0;
    while index < Channel::ALL.len() {
        let channel = Channel::ALL[index];
        let name = channel.as_str().as_bytes();
        let text = text.as_bytes();
        if name.len() == text.len() {
            let mut byte = 0;
            while byte < name.len() && name[byte] == text[byte] {
                byte += 1;
            }
            if byte == name.len() {
                return Some(channel);
            }
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a commit time, only a higher version is newer. By number
    /// alone, a later stable patch is older than the next minor's nightly,
    /// whatever their commits.
    #[test]
    fn a_later_commit_is_newer_whatever_the_version() {
        let build = |number: &str, committed_at_ms| Version {
            number: number.into(),
            commit: None,
            committed_at_ms,
            channel: None,
        };
        let nightly = build("0.3.0-nightly.20261125", Some(2));
        let stable = build("0.2.1", Some(3));
        assert!(stable.is_newer_than(&nightly));
        assert!(!nightly.is_newer_than(&stable));
        assert!(build("0.2.1", None).is_newer_than(&build("0.2.0", None)));
        assert!(!build("0.2.0", None).is_newer_than(&build("0.2.0", Some(1))));
        assert!(!build("0.3.0", Some(1)).is_newer_than(&build("0.4.0-dev", None)));
        assert!(build("0.3.0", None).is_newer_than(&build("0.3.0-nightly.20261125", None)));
        assert!(
            !Version::numbered("0.4.1").is_newer_than(&Version::numbered("0.5.0-nightly.20261125"))
        );
    }

    #[test]
    fn stamps_parse_as_release_builds_write_them() {
        assert_eq!(parse_seconds("1764028800"), 1_764_028_800);
        assert_eq!(parse_channel("nightly"), Some(Channel::Nightly));
        assert_eq!(parse_channel("weekly"), None);
    }
}
