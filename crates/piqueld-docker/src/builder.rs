//! A build machine's `BuildKit` daemon, reached through Docker Buildx.
//!
//! Buildx's `remote` driver sends a build's context to the daemon, which
//! builds the image and pushes it to the build registry under a tag naming
//! this installation (see [`crate::BuildRegistry::new_tag`]). The engine then
//! pulls it from there by digest, like any registry image:
//!
//! ```text
//! docker buildx build --builder=piqueld --platform=linux/arm64
//!     --output=type=image,name=registry.example.ts.net:5000/piqueld:<instance ID>.<UUIDv7>,push=true …
//! registry.example.ts.net:5000/piqueld@sha256:<64 hex>
//! ```
use crate::command::CommandError;
use piqueld_core::build_machine::{BuilderEndpoint, RemoteBuild};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// A build machine whose `BuildKit` daemon did not accept a connection
/// before a build.
#[derive(Debug, thiserror::Error)]
#[error("the build machine's BuildKit daemon at {address} is unreachable")]
pub struct BuilderUnavailable {
    /// The `host:port` tried.
    pub(crate) address: String,
    /// Why connecting failed.
    #[source]
    pub(crate) source: std::io::Error,
}

/// Why a build machine could not be registered with Buildx.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The build registry's HTTP client could not be created.
    #[error("create the build registry's HTTP client")]
    Registry(#[source] reqwest::Error),
    /// The earlier Buildx configuration could not be removed.
    #[error("remove the earlier Buildx configuration")]
    Remove(#[source] std::io::Error),
    /// Buildx refused the registration.
    #[error(transparent)]
    Buildx(#[from] CommandError),
}

/// A build machine registered with Buildx, in a configuration directory of
/// piqueld's own: nothing depends on the daemon user's Docker configuration,
/// or changes it.
#[derive(Debug)]
pub struct RemoteBuilder {
    /// Buildx's configuration directory, `<data_dir>/buildx`.
    config: PathBuf,
    /// Where its `BuildKit` daemon listens.
    endpoint: BuilderEndpoint,
    /// Where the build machine pushes images, and a client of that
    /// registry.
    pub(super) registry: crate::BuildRegistry,
    /// The latest check of its `BuildKit` daemon; see [`Self::probe`].
    last_check: parking_lot::Mutex<Option<piqueld_core::api::BuilderCheck>>,
}

impl RemoteBuilder {
    /// The data directory's entry holding Buildx's registration of the build
    /// machine.
    pub const DIRECTORY: &str = "buildx";

    /// The builder's name in Buildx.
    const NAME: &str = "piqueld";
    /// Longest a connection to the build machine may take before it counts
    /// as unreachable. Buildx itself waits about 20 seconds.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Registers `remote`'s `BuildKit` daemon in `<data_dir>/buildx`, replacing
    /// what an earlier start registered. Buildx only records the endpoint, so
    /// the build machine need not be reachable.
    ///
    /// # Errors
    /// Returns why the directory could not be replaced or Buildx refused.
    pub async fn register(remote: &RemoteBuild, data_dir: &Path) -> Result<Self, RegisterError> {
        let builder = Self {
            config: data_dir.join(Self::DIRECTORY),
            endpoint: remote.builder.clone(),
            registry: crate::BuildRegistry::new(remote.repository.clone())
                .map_err(RegisterError::Registry)?,
            last_check: parking_lot::Mutex::default(),
        };
        match tokio::fs::remove_dir_all(&builder.config).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(RegisterError::Remove(error));
            }
            _ => {}
        }
        let mut create = builder.docker();
        create
            .args(["buildx", "create", "--driver=remote", "--name", Self::NAME])
            .arg(remote.builder.to_string());
        crate::command::LoggedCommand::run_recorded(&mut create, "register the builder", None)
            .await?;
        Ok(builder)
    }

    /// Checks that the build machine's `BuildKit` daemon accepts a
    /// connection, as before a build; see [`Self::connect`].
    ///
    /// # Errors
    /// Returns why it could not be reached.
    pub(crate) async fn probe(&self) -> Result<(), BuilderUnavailable> {
        self.connect().await.1
    }

    /// Checks the build machine's `BuildKit` daemon on request, returning
    /// this check; see [`Self::connect`].
    pub async fn check(&self) -> piqueld_core::api::BuilderCheck {
        self.connect().await.0
    }

    /// Connects to the build machine's `BuildKit` daemon, waiting up to
    /// [`Self::PROBE_TIMEOUT`], and keeps the check as the latest for
    /// [`Self::status`]. It checks only `BuildKit`: the build registry is
    /// reached by the engine, and by image cleanup.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    async fn connect(
        &self,
    ) -> (
        piqueld_core::api::BuilderCheck,
        Result<(), BuilderUnavailable>,
    ) {
        let address = self.endpoint.address();
        let result = tokio::time::timeout(Self::PROBE_TIMEOUT, Self::reach(address))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no answer within {} seconds", Self::PROBE_TIMEOUT.as_secs()),
                )
            })
            .flatten()
            .map(drop)
            .map_err(|source| BuilderUnavailable {
                address: address.to_owned(),
                source,
            });
        let check = piqueld_core::api::BuilderCheck {
            reachable: result.is_ok(),
            checked_at_ms: crate::now_ns() / 1_000_000,
            message: result.as_ref().err().map(|error| error.source.to_string()),
        };
        *self.last_check.lock() = Some(check.clone());
        (check, result)
    }

    /// Where the build machine is, and its latest check.
    #[must_use]
    pub fn status(&self) -> piqueld_core::api::BuilderStatus {
        piqueld_core::api::BuilderStatus {
            endpoint: self.endpoint.to_string(),
            repository: self.registry.repository.to_string(),
            last_check: self.last_check.lock().clone(),
        }
    }

    /// Connects to `address`, trying every address it resolves to at once,
    /// as Buildx's dialer falls back from one to the next: one that drops
    /// packets, such as an unrouted IPv6 address, must not use up the
    /// timeout while another answers.
    async fn reach(address: &str) -> std::io::Result<()> {
        let attempts: Vec<_> = tokio::net::lookup_host(address)
            .await?
            .map(|resolved| Box::pin(tokio::net::TcpStream::connect(resolved)))
            .collect();
        if attempts.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the address resolves to nothing",
            ));
        }
        futures_util::future::select_ok(attempts).await.map(drop)
    }

    /// A `docker` command that reads this builder's Buildx configuration.
    pub(super) fn docker(&self) -> Command {
        let mut command = Command::new("docker");
        command.env("BUILDX_CONFIG", &self.config);
        command
    }

    /// The `docker buildx build` options that run a build on the build
    /// machine for `platform`, the engine's, and push the image under `tag`.
    pub(super) fn options(&self, platform: &str, tag: &str) -> [String; 3] {
        [
            format!("--builder={}", Self::NAME),
            format!("--platform={platform}"),
            format!(
                "--output=type=image,name={}:{tag},push=true",
                self.registry.repository
            ),
        ]
    }

    /// The repository the build machine pushes to, and its registry.
    #[must_use]
    pub(crate) fn registry(&self) -> &crate::BuildRegistry {
        &self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_core::build_machine::BuildRepository;

    /// A builder whose `BuildKit` daemon listens at `daemon`.
    fn builder(daemon: std::net::SocketAddr) -> RemoteBuilder {
        RemoteBuilder {
            config: PathBuf::from("/var/lib/piqueld/buildx"),
            endpoint: BuilderEndpoint::try_from(format!("tcp://{daemon}"))
                .expect("the builder endpoint parses"),
            registry: crate::BuildRegistry::new(
                BuildRepository::try_from("registry.example.ts.net:5000/piqueld".to_owned())
                    .expect("the builder repository parses"),
            )
            .expect("the build registry builds"),
            last_check: parking_lot::Mutex::default(),
        }
    }

    #[test]
    fn builds_run_on_the_builder_for_the_engines_platform_and_push_under_their_tag() {
        assert_eq!(
            builder("127.0.0.1:1234".parse().expect("the address parses"))
                .options("linux/arm64", "instance-1.0a"),
            [
                "--builder=piqueld",
                "--platform=linux/arm64",
                "--output=type=image,name=registry.example.ts.net:5000/piqueld:instance-1.0a,push=true",
            ]
        );
    }

    #[tokio::test]
    async fn a_build_machine_is_reachable_once_its_daemon_accepts_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the listener binds");
        let reachable = builder(listener.local_addr().expect("the listener has an address"));
        assert_eq!(reachable.status().last_check, None);
        reachable.probe().await.expect("the probe runs");
        assert!(
            reachable
                .status()
                .last_check
                .expect("the probe recorded a check")
                .reachable
        );
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the listener binds")
            .local_addr()
            .expect("the listener has an address");
        let unreachable = builder(closed);
        let error = unreachable
            .probe()
            .await
            .expect_err("the unreachable builder fails its probe");
        assert_eq!(
            error.to_string(),
            format!("the build machine's BuildKit daemon at {closed} is unreachable")
        );
        let check = unreachable
            .status()
            .last_check
            .expect("the probe recorded a check");
        assert!(!check.reachable);
        assert_eq!(check.message, Some(error.source.to_string()));
    }
}
