//! The host's Docker engine, and the Docker-in-Docker engines xtask runs on it
//! so tests and development instances never touch the host's own containers.

use std::fmt::Display;
use std::net::Ipv4Addr;
use std::ops::Deref;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ExecConfig, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectContainerOptions,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};
use tokio::net::TcpStream;

use crate::process::checked_stdout;

/// The pinned Docker-in-Docker image; `PIQUELD_DIND_IMAGE` overrides it.
/// Pulled from [`REGISTRY_MIRROR`] alone; the digest still verifies it.
const DIND_IMAGE: &str = "mirror.gcr.io/library/docker:29.6.2-dind@sha256:bfec1f5159c63a81ca6fdedbd81404d2c0e16378ed0feec3bb3fbf3998847659";

/// Google's public mirror of Docker Hub. CI runners share their addresses, so
/// their anonymous Docker Hub pulls run into its rate limit; the mirror needs
/// no credentials. Docker-in-Docker engines pull through it, and from Docker
/// Hub when it lacks an image.
const REGISTRY_MIRROR: &str = "https://mirror.gcr.io";

/// How often to check that the Docker-in-Docker engine answers.
const DIND_POLL: Duration = Duration::from_secs(1);

/// How often to check that a container accepts connections on its port.
const PORT_POLL: Duration = Duration::from_millis(250);

/// How long a container may take to become ready, requests included.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long reading a container's output may take after it failed to become ready.
const LOGS_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a Docker-in-Docker engine listens inside its container, onto which
/// [`Engine::dind_container`] bind-mounts a private host directory.
const DIND_SOCKET_DIR: &str = "/piqueld-socket";

/// A Docker engine client, connected to the host's engine by [`Engine::host`].
pub(crate) struct Engine(Docker);

impl Deref for Engine {
    type Target = Docker;

    fn deref(&self) -> &Docker {
        &self.0
    }
}

impl Engine {
    /// Connects to the engine the Docker CLI uses: `DOCKER_HOST`, else the
    /// current context, else `/var/run/docker.sock`. It must be local, since
    /// xtask bind-mounts host directories into its containers.
    pub(crate) async fn host() -> Result<Self> {
        let endpoint = match std::env::var("DOCKER_HOST") {
            Ok(host) => host,
            Err(_) => Self::context_endpoint()?,
        };
        let path = endpoint.strip_prefix("unix://").with_context(|| {
            format!("xtask needs a local Unix-socket Docker endpoint, found {endpoint}")
        })?;
        let docker = Docker::connect_with_unix(path, 120, bollard::API_DEFAULT_VERSION)?;
        docker
            .ping()
            .await
            .with_context(|| format!("no running Docker engine at {endpoint}"))?;
        Ok(Self(docker))
    }

    /// The endpoint of the Docker CLI's current context; the default socket when there is no CLI.
    fn context_endpoint() -> Result<String> {
        let output = std::process::Command::new("docker")
            .args([
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ])
            .output();
        match output {
            Ok(output) => checked_stdout("docker context inspect", output)
                .context("could not resolve the Docker context"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok("unix:///var/run/docker.sock".to_owned())
            }
            Err(error) => Err(error).context("run the Docker CLI"),
        }
    }

    /// The Docker-in-Docker image: `PIQUELD_DIND_IMAGE` if set, else the pinned one.
    pub(crate) fn dind_image() -> String {
        std::env::var("PIQUELD_DIND_IMAGE").unwrap_or_else(|_| DIND_IMAGE.to_owned())
    }

    /// A privileged Docker-in-Docker container from `image`, whose engine
    /// listens in `socket_dir`, a private host directory, as `docker.sock`,
    /// and pulls through [`REGISTRY_MIRROR`]. `binds` mounts more host paths
    /// or volumes.
    pub(crate) fn dind_container(
        image: String,
        socket_dir: impl Display,
        binds: impl IntoIterator<Item = String>,
    ) -> ContainerCreateBody {
        ContainerCreateBody {
            image: Some(image),
            cmd: Some(vec![
                "dockerd".to_owned(),
                format!("--host=unix://{DIND_SOCKET_DIR}/docker.sock"),
                format!("--registry-mirror={REGISTRY_MIRROR}"),
            ]),
            env: Some(vec!["DOCKER_TLS_CERTDIR=".to_owned()]),
            host_config: Some(HostConfig {
                privileged: Some(true),
                binds: Some(
                    std::iter::once(format!("{socket_dir}:{DIND_SOCKET_DIR}"))
                        .chain(binds)
                        .collect(),
                ),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        }
    }

    /// Pulls `image` unless the engine has it.
    pub(crate) async fn pull_missing(&self, image: &str) -> Result<()> {
        if self.inspect_image(image).await.is_err() {
            let options = CreateImageOptionsBuilder::new().from_image(image).build();
            self.create_image(Some(options), None, None)
                .try_collect::<Vec<_>>()
                .await
                .with_context(|| format!("pull {image}"))?;
        }
        Ok(())
    }

    /// Creates and starts a container, returning its ID.
    pub(crate) async fn launch(
        &self,
        name: Option<&str>,
        body: ContainerCreateBody,
    ) -> Result<String> {
        let image = body.image.clone().unwrap_or_default();
        let options = name.map(|name| CreateContainerOptionsBuilder::new().name(name).build());
        let id = self
            .create_container(options, body)
            .await
            .with_context(|| format!("create a container from {image}"))?
            .id;
        self.start_container(&id, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("start a container from {image}"))?;
        Ok(id)
    }

    /// Runs a container to completion, then removes it.
    pub(crate) async fn run_once(&self, body: ContainerCreateBody) -> Result<()> {
        let id = self.launch(None, body).await?;
        let result = self
            .wait_container(&id, None::<WaitContainerOptions>)
            .try_collect::<Vec<_>>()
            .await;
        self.remove(&id).await?;
        result.context("run a container")?;
        Ok(())
    }

    /// Removes a container and its anonymous volumes, if it exists.
    pub(crate) async fn remove(&self, container: &str) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        Self::found(self.remove_container(container, Some(options)).await)
            .with_context(|| format!("remove container {container}"))?;
        Ok(())
    }

    /// The outcome of a request about a container or volume, with one that
    /// doesn't exist as `None`.
    pub(crate) fn found<T>(
        result: Result<T, bollard::errors::Error>,
    ) -> Result<Option<T>, bollard::errors::Error> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether the container `container` is running; fails when it can't be inspected.
    pub(crate) async fn running(&self, container: &str) -> Result<bool> {
        let state = self
            .inspect_container(container, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("inspect container {container}"))?
            .state;
        Ok(state.and_then(|state| state.running).unwrap_or(false))
    }

    /// Runs `cmd` in a running container and returns its exit code.
    async fn exec(&self, container: &str, cmd: &[&str]) -> Result<i64> {
        let config = ExecConfig {
            cmd: Some(cmd.iter().map(ToString::to_string).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..ExecConfig::default()
        };
        let exec = self.create_exec(container, config).await?.id;
        if let bollard::exec::StartExecResults::Attached { output, .. } =
            self.start_exec(&exec, None).await?
        {
            output.for_each(|_| async {}).await;
        }
        Ok(self.inspect_exec(&exec).await?.exit_code.unwrap_or(-1))
    }

    /// Prints a container's output, for diagnosing startup failures.
    pub(crate) async fn print_logs(&self, container: &str) {
        let options = LogsOptionsBuilder::new().stdout(true).stderr(true).build();
        let mut logs = self.logs(container, Some(options));
        while let Some(Ok(output)) = logs.next().await {
            eprint!("{output}");
        }
    }

    /// Waits up to 60 seconds for the Docker-in-Docker engine in `container`,
    /// then gives its socket to the host user, in their private directory.
    pub(crate) async fn wait_for_dind(&self, container: &str) -> Result<()> {
        let ready = format!(
            "docker -H unix://{DIND_SOCKET_DIR}/docker.sock info >/dev/null 2>&1 \
             && chown {uid} {DIND_SOCKET_DIR}/docker.sock",
            uid = rustix::process::getuid().as_raw()
        );
        self.wait_until(container, "the Docker daemon", DIND_POLL, || async {
            self.exec(container, &["sh", "-c", &ready]).await.ok() == Some(0)
        })
        .await
    }

    /// Waits up to 60 seconds for `container`, on the host network, to accept
    /// connections on `port` on loopback.
    pub(crate) async fn wait_for_port(&self, container: &str, port: u16) -> Result<()> {
        let what = format!("a listener on port {port}");
        self.wait_until(container, &what, PORT_POLL, || async {
            TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
        })
        .await
    }

    /// Checks `ready` every `interval` for up to 60 seconds, failing as soon
    /// as `container` exits. `what` names what it waits for. On failure it
    /// prints the container's output. The limit covers every request, so a
    /// stalled engine or probe can't stretch the wait.
    async fn wait_until<F: Future<Output = bool>>(
        &self,
        container: &str,
        what: &str,
        interval: Duration,
        mut ready: impl FnMut() -> F,
    ) -> Result<()> {
        let poll = async {
            loop {
                if !self.running(container).await? {
                    return Ok::<_, anyhow::Error>(false);
                }
                if ready().await {
                    return Ok(true);
                }
                tokio::time::sleep(interval).await;
            }
        };
        let failure = match tokio::time::timeout(READY_TIMEOUT, poll).await {
            Ok(Ok(true)) => return Ok(()),
            Ok(Ok(false)) => format!("container {container} exited before {what} was ready"),
            Ok(Err(error)) => return Err(error),
            Err(_) => format!("{what} in container {container} was not ready within 60 seconds"),
        };
        if tokio::time::timeout(LOGS_TIMEOUT, self.print_logs(container))
            .await
            .is_err()
        {
            eprintln!("gave up reading the output of container {container} after 10 seconds");
        }
        bail!("{failure}")
    }
}
