//! Where images build: this host's Docker Engine, or a build machine.
use super::schema_from_raw;
use piqueld_core::build_machine::{BuildRepository, BuilderEndpoint, RemoteBuild, WhenUnavailable};
use serde::Deserialize;

/// The `[build]` table. Images build on this host's Docker Engine unless it
/// names a build machine. Its schema is that of its TOML form,
/// `RawBuildConfig`.
///
/// ```toml
/// [build]
/// builder = "tcp://build.example.ts.net:1234"
/// repository = "registry.example.ts.net:5000/piqueld"
/// unavailable = "fail" # or "local"
/// ```
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawBuildConfig")]
pub struct BuildConfig {
    /// The build machine images build on, if any.
    pub remote: Option<RemoteBuild>,
}

/// A build machine that builds this installation's images instead of
/// this host. piqueld still checks repositories out here, and sends each build
/// to the machine's `BuildKit` daemon, which pushes the image to a registry
/// this host's Docker Engine pulls it from. Set `builder` and `repository`
/// together, or neither: without them, images build on this host's Docker
/// Engine.
//
// The TOML form of [`BuildConfig`].
#[derive(Default, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
struct RawBuildConfig {
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
        Ok(Self { remote })
    }
}

schema_from_raw!(BuildConfig => RawBuildConfig);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<BuildConfig, toml::de::Error> {
        toml::from_str(toml)
    }

    #[test]
    fn a_build_machine_needs_its_builder_and_its_repository() {
        assert_eq!(parse("").unwrap(), BuildConfig::default());
        let remote = parse(
            "builder = 'tcp://build.example.ts.net:1234'\n\
             repository = 'build.example.ts.net:5000/piqueld'",
        )
        .unwrap()
        .remote
        .unwrap();
        assert_eq!(
            remote.builder.to_string(),
            "tcp://build.example.ts.net:1234"
        );
        assert_eq!(
            remote.repository.to_string(),
            "build.example.ts.net:5000/piqueld"
        );
        for invalid in [
            "builder = 'tcp://build.example.ts.net:1234'",
            "repository = 'build.example.ts.net:5000/piqueld'",
            // The policy without a build machine.
            "unavailable = 'local'",
            // An invalid builder, and an invalid repository.
            "builder = 'tcp://build.example.ts.net'\nrepository = 'registry.example/piqueld'",
            "builder = 'tcp://build.example:1234'\nrepository = 'piqueld'",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
    }
}
