//! A build machine: another machine whose `BuildKit` daemon builds this
//! installation's images and pushes them to a registry, from which this
//! host's Docker Engine pulls them. The daemon's configuration names it, and
//! its Docker boundary builds on it.
use crate::{RepositoryDigest, Sha256Digest, nodes::TcpEndpoint};
use serde::{Deserialize, Serialize};

/// A build machine: its `BuildKit` daemon, and where that pushes the images
/// this host then pulls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBuild {
    /// The `BuildKit` daemon that runs the builds.
    pub builder: TcpEndpoint,
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
    /// The image this repository holds under `digest`.
    ///
    /// # Panics
    /// Never: a validated repository and a digest form a repository digest.
    #[must_use]
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
mod tests {
    use super::*;

    #[test]
    fn a_build_repository_is_on_a_named_registry_without_a_tag() {
        let repository =
            BuildRepository::try_from("build.example.ts.net:5000/piqueld".to_owned()).unwrap();
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(
            repository.image(&digest).as_str(),
            format!("build.example.ts.net:5000/piqueld@{digest}")
        );
        // Docker Hub's, a tag, and upper case.
        for invalid in [
            "piqueld",
            "registry.example/piqueld:v1",
            "registry.example/Piqueld",
        ] {
            assert!(
                BuildRepository::try_from(invalid.to_owned()).is_err(),
                "{invalid}"
            );
        }
    }
}
