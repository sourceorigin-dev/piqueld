//! A build machine: another machine whose `BuildKit` daemon builds this
//! installation's images and pushes them to a registry, from which this
//! host's Docker Engine pulls them. The daemon's configuration names it, and
//! its Docker boundary builds on it.
use crate::{RepositoryDigest, Sha256Digest};
use serde::{Deserialize, Serialize};

/// A build machine: its `BuildKit` daemon, and where that pushes the images
/// this host then pulls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBuild {
    /// The `BuildKit` daemon that runs the builds.
    pub builder: BuilderEndpoint,
    /// The image repository the builds are pushed to.
    pub repository: BuildRepository,
    /// What a build does while the build machine can't be reached.
    pub unavailable: WhenUnavailable,
}

/// What a build does while its build machine can't be reached.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WhenUnavailable {
    /// Fail the deployment with `builder_unavailable`.
    #[default]
    Fail,
    /// Build on this host's Docker Engine instead.
    Local,
}

/// Where a build machine's `BuildKit` daemon listens: `tcp://host:port`,
/// without credentials or a path, which `BuildKit` would not use. Displays
/// as configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct BuilderEndpoint(String);

impl TryFrom<String> for BuilderEndpoint {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let address = value
            .strip_prefix("tcp://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest));
        // An IPv6 host is bracketed, so its port follows the last colon.
        let valid = address
            .and_then(|address| address.rsplit_once(':'))
            .is_some_and(|(host, port)| {
                !host.is_empty()
                    && !host.contains(['/', '@', '?', '#', ' '])
                    && (!host.contains(':') || host.starts_with('[') && host.ends_with(']'))
                    && port.parse::<u16>().is_ok()
            });
        if valid {
            Ok(Self(value))
        } else {
            Err("build.builder must be tcp://host:port")
        }
    }
}

impl BuilderEndpoint {
    /// The daemon's `host:port`, to connect to.
    #[must_use]
    pub fn address(&self) -> &str {
        let address = self.0.strip_prefix("tcp://").unwrap_or(&self.0);
        address.strip_suffix('/').unwrap_or(address)
    }
}

impl std::fmt::Display for BuilderEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The image repository a build machine pushes to, on a named registry:
/// `registry-host[:port]/path`. Displays as configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct BuildRepository(String);

impl TryFrom<String> for BuildRepository {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        // Only a repository on an explicit registry, without a tag or
        // digest, is its own canonical form: `piqueld` would be Docker Hub's.
        if crate::image_repository(&value).as_deref() == Some(value.as_str()) {
            Ok(Self(value))
        } else {
            Err("build.repository must be registry-host[:port]/path, in lower case, without a tag")
        }
    }
}

impl BuildRepository {
    /// The registry holding it, as `host[:port]`.
    #[must_use]
    pub fn registry(&self) -> &str {
        self.split().0
    }

    /// Its path on the registry, such as `piqueld`.
    #[must_use]
    pub fn path(&self) -> &str {
        self.split().1
    }

    /// The registry and the path: a validated repository names its registry.
    fn split(&self) -> (&str, &str) {
        self.0.split_once('/').unwrap_or(("", &self.0))
    }

    /// The digest of `image`, if it is in this repository.
    #[must_use]
    pub fn digest_of(&self, image: &RepositoryDigest) -> Option<Sha256Digest> {
        let digest = image
            .as_str()
            .strip_prefix(self.0.as_str())?
            .strip_prefix('@')?;
        Sha256Digest::parse(digest).ok()
    }

    /// The image this repository holds under `digest`.
    ///
    /// # Panics
    /// Never: a validated repository and a digest form a repository digest.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "A validated repository and a digest always form a repository digest."
    )]
    pub fn image(&self, digest: &Sha256Digest) -> RepositoryDigest {
        RepositoryDigest::parse(format!("{}@{digest}", self.0))
            .expect("a validated repository and a digest form a repository digest")
    }
}

impl std::fmt::Display for BuildRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests;
