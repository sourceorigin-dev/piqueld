//! Opt-in hardening of a service's containers.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use utoipa::ToSchema;

/// Opt-in hardening of the service's containers and its jobs'. Every setting
/// is off by default, and changing one redeploys the service.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceSecurity {
    /// Numeric user, and optionally group, the containers run as instead of
    /// the image's: `uid` or `uid:gid`, such as `1000:1000`, each at most
    /// 2147483647. A service that
    /// receives telemetry can't set it, since it runs as the daemon's user.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "1000:1000")]
    pub user: Option<ContainerUser>,
    /// Whether the containers' root filesystem is read-only. Volumes and
    /// files mount as usual, and `/tmp` is a writable in-memory directory,
    /// so nothing can mount at `/tmp` itself.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub read_only_root: bool,
    /// Whether the containers run without any Linux capability, so even
    /// their root user can't bypass file permissions or change file owners.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub drop_capabilities: bool,
    /// Whether the containers' processes can't gain privileges, such as
    /// through setuid programs like `sudo`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_new_privileges: bool,
}

impl ServiceSecurity {
    /// Where a read-only root keeps a writable in-memory directory.
    pub const TMPFS_TARGET: &str = "/tmp";

    /// Whether no setting is on; such blocks are omitted on export.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A numeric user and optional group, written `uid` or `uid:gid`, each from
/// 0 to [`Self::MAX_ID`]. Names aren't accepted, since they depend on each
/// image's `/etc/passwd`. Only parsing builds one, so every value is in range.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContainerUser {
    /// User ID.
    uid: u32,
    /// Group ID; the image's group of the user when unset.
    gid: Option<u32>,
}

impl ContainerUser {
    /// The largest ID Docker accepts: it resolves a numeric user or group
    /// missing from the image's `/etc/passwd` or `/etc/group` only up to
    /// this, and fails the container's start above it.
    pub const MAX_ID: u32 = 2_147_483_647;
}

impl FromStr for ContainerUser {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        const ERROR: &str =
            "a user is a numeric uid or uid:gid from 0 to 2147483647, such as 1000:1000";
        let id = |id: &str| {
            id.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| id.parse().ok())
                .flatten()
                .filter(|id| *id <= Self::MAX_ID)
                .ok_or(ERROR)
        };
        let (uid, gid) = match value.split_once(':') {
            Some((uid, gid)) => (uid, Some(gid)),
            None => (value, None),
        };
        Ok(Self {
            uid: id(uid)?,
            gid: gid.map(id).transpose()?,
        })
    }
}

impl TryFrom<String> for ContainerUser {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ContainerUser> for String {
    fn from(user: ContainerUser) -> Self {
        user.to_string()
    }
}

impl fmt::Display for ContainerUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.uid)?;
        match self.gid {
            Some(gid) => write!(f, ":{gid}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ContainerUser;

    #[test]
    fn users_are_numeric_ids() {
        // Docker resolves IDs only up to `i32::MAX`.
        for valid in ["0", "1000", "1000:1000", "65534:0", "2147483647:2147483647"] {
            assert_eq!(valid.parse::<ContainerUser>().unwrap().to_string(), valid);
        }
        for invalid in [
            "",
            "root",
            "1000:",
            ":1000",
            "+1",
            "1000:1000:1",
            "2147483648",
            "0:2147483648",
            "4294967296",
        ] {
            assert!(invalid.parse::<ContainerUser>().is_err(), "{invalid}");
        }
    }
}
