//! Where images build, this host's Docker Engine or a build machine, and
//! how many build at once.
use super::schema_from_raw;
use piqueld_core::build_machine::{BuildRepository, BuilderEndpoint, RemoteBuild, WhenUnavailable};
use serde::Deserialize;
use std::num::NonZeroU8;

/// The `[build]` table. Images build on this host's Docker Engine unless it
/// names a build machine. Its schema is that of its TOML form,
/// `RawBuildConfig`.
///
/// ```toml
/// [build]
/// parallel = 2
/// builder = "tcp://build.example.ts.net:1234"
/// repository = "registry.example.ts.net:5000/piqueld"
/// unavailable = "fail" # or "local"
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawBuildConfig")]
pub struct BuildConfig {
    /// How many images build at once.
    pub parallel: NonZeroU8,
    /// The build machine images build on, if any.
    pub remote: Option<RemoteBuild>,
}

impl BuildConfig {
    /// How many images build at once unless `parallel` says otherwise.
    const DEFAULT_PARALLEL: NonZeroU8 = NonZeroU8::MIN;
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            parallel: Self::DEFAULT_PARALLEL,
            remote: None,
        }
    }
}

/// How many images build at once, and where: on this host's Docker Engine,
/// or on a build machine instead. piqueld still checks repositories out here, and sends each build
/// to the machine's `BuildKit` daemon, which pushes the image to a registry
/// this host's Docker Engine pulls it from. Set `builder` and `repository`
/// together, or neither: without them, images build on this host's Docker
/// Engine.
//
// The TOML form of [`BuildConfig`].
#[derive(Default, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
struct RawBuildConfig {
    /// How many images build at once, on this host or the build machine.
    /// Further builds wait, environments' before previews', and the wait
    /// counts against `reconciliation.prepare_timeout_seconds`. Each build
    /// at once needs its own CPU, memory and disk, so raise it only as far
    /// as the builder can take.
    #[schema(value_type = Option<u8>, minimum = 1, maximum = 255, default = 1)]
    parallel: Option<NonZeroU8>,
    /// The build machine's `BuildKit` daemon, as `tcp://host:port`. The
    /// connection is neither authenticated nor encrypted, so only this host
    /// should be able to reach that port, over a network that encrypts its
    /// traffic, such as a tailnet whose access rules allow nothing else.
    #[schema(value_type = Option<String>)]
    builder: Option<BuilderEndpoint>,
    /// The image repository the build machine pushes to and this host pulls
    /// from, as `registry-host[:port]/path`, without a tag. Both reach it
    /// under this name, without credentials, and Docker requires HTTPS of a
    /// registry on another machine. Anyone who can reach the registry can
    /// pull every image, build arguments included, or push to it, so only
    /// this host and the build machine should be able to. Every image goes
    /// to this one repository, so they share their layers, under a tag
    /// naming this installation, by which image cleanup finds it.
    #[schema(value_type = Option<String>)]
    repository: Option<BuildRepository>,
    /// What a deployment that has to build does while the build machine
    /// can't be reached: `fail`, the default, fails it with
    /// `builder_unavailable`, and `local` builds on this host's Docker Engine
    /// instead, where a build competes with the applications the host runs.
    /// Requires `builder`.
    unavailable: Option<WhenUnavailable>,
}

impl TryFrom<RawBuildConfig> for BuildConfig {
    type Error = &'static str;
    fn try_from(raw: RawBuildConfig) -> Result<Self, Self::Error> {
        let remote = match (raw.builder, raw.repository) {
            (None, None) if raw.unavailable.is_some() => {
                return Err("build.unavailable requires build.builder");
            }
            (None, None) => None,
            (Some(builder), Some(repository)) => Some(RemoteBuild {
                builder,
                repository,
                unavailable: raw.unavailable.unwrap_or_default(),
            }),
            (Some(_), None) => return Err("build.builder requires build.repository"),
            (None, Some(_)) => return Err("build.repository requires build.builder"),
        };
        Ok(Self {
            parallel: raw.parallel.unwrap_or(Self::DEFAULT_PARALLEL),
            remote,
        })
    }
}

schema_from_raw!(BuildConfig => RawBuildConfig);

#[cfg(test)]
mod tests;
