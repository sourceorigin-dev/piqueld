//! `just dev`: one isolated development instance per worktree, with its own
//! configuration, data and runtime directories, localhost port, and Docker
//! engine, so worktrees never share state. See docs/dev/development.md.

mod supervisor;

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, RemoveVolumeOptions,
};
use clap::Subcommand;
use rustix::process::getuid;
use serde::{Deserialize, Serialize};

use crate::Workspace;
use crate::docker::Engine;
use supervisor::{Holder, Supervisor};

/// How long a query of the daemon's API may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// How often `just dev wait` checks the instance.
const WAIT_POLL: Duration = Duration::from_secs(2);

/// The instance's configuration file, in the workspace root.
const CONFIG_FILE: &str = "piqueld.local.toml";
/// Held every user's runtime directories before [`Instance::runtime_root`];
/// `just dev config --force` moves a configuration that still uses it.
const LEGACY_RUNTIME_ROOT: &str = "/tmp/piqueld-dev";
/// The end of the socket path of the shared tailnet node that instances
/// served on before T3 Code's port forwarding replaced it.
const LEGACY_TAILNET_SOCKET: &str = "tailnet/tailscaled.sock";
/// Changes to these rebuild and restart the daemon. It embeds the dashboard and
/// the migrations at compile time, so they are watched too, as are the build
/// settings, and it reads its configuration at startup.
const SOURCES: [&str; 8] = [
    "apps/piqueld",
    "apps/piqueld-ui",
    "crates",
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    "rust-toolchain.toml",
    CONFIG_FILE,
];
/// Labels on each instance's engine, which let `prune` find what to delete.
const WORKTREE_LABEL: &str = "io.piqueld.dev.worktree";
/// The label on an engine's container naming its worktree.
const DATA_LABEL: &str = "io.piqueld.dev.data";
/// The label on an engine's container naming its runtime directory.
const RUNTIME_LABEL: &str = "io.piqueld.dev.runtime";

/// The `just dev` subcommands.
#[derive(Subcommand)]
pub(crate) enum Command {
    /// Watch the sources, rebuilding and restarting the daemon on every change.
    Run,
    /// Run in the background, then wait until ready.
    Start {
        /// Seconds to wait.
        #[arg(default_value_t = 900)]
        timeout: u64,
    },
    /// Wait until the current sources are built and serving.
    Wait {
        /// Seconds to wait.
        #[arg(default_value_t = 900)]
        timeout: u64,
    },
    /// Print the instance's state, URL, and files.
    Status,
    /// Stop the daemon and the watcher.
    Stop,
    /// Stop, then delete the instance's Docker engine and state.
    Clean,
    /// Stop and clean the instances of worktrees that no longer exist.
    Prune,
    /// Write piqueld.local.toml unless it exists.
    Config {
        /// Replace an existing configuration.
        #[arg(long)]
        force: bool,
    },
    /// Run this worktree's piquelctl against the instance.
    #[command(disable_help_flag = true)]
    Ctl {
        /// Arguments for `piquelctl`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl Command {
    /// Runs the command for the worktree `workspace`, failing with the task's error.
    pub(crate) async fn run(self, workspace: Workspace) -> Result<ExitCode> {
        match self {
            Self::Config { force } => {
                if !Instance::configure(&workspace, force)? {
                    eprintln!("kept {CONFIG_FILE}; pass --force to replace it");
                }
            }
            Self::Prune => Engine::host().await?.prune_dev_instances().await?,
            Self::Run => Instance::load_or_configure(workspace)?.run().await?,
            Self::Start { timeout } => {
                let instance = Instance::load_or_configure(workspace)?;
                instance.start().await?;
                instance.wait(timeout).await?;
            }
            Self::Wait { timeout } => Instance::load(workspace)?.wait(timeout).await?,
            Self::Status => Instance::load(workspace)?.status().await,
            Self::Stop => Instance::load(workspace)?.supervisor().stop().await?,
            Self::Clean => Instance::load(workspace)?.clean().await?,
            Self::Ctl { args } => return Err(Instance::load(workspace)?.ctl(&args)),
        }
        Ok(ExitCode::SUCCESS)
    }
}

/// The parts of piqueld.local.toml the tasks read; the rest is the daemon's.
/// Edits to the file are respected.
#[derive(Deserialize)]
struct Config {
    /// The daemon's server settings.
    server: ServerConfig,
    /// The Docker engine the daemon uses.
    #[serde(default)]
    docker: DockerConfig,
    /// The tailnet settings, which are only read to notice an old configuration.
    #[serde(default)]
    tailscale: TailscaleConfig,
}

/// The `[server]` settings the tasks read.
#[derive(Deserialize)]
struct ServerConfig {
    /// Where the daemon keeps its data.
    data_dir: PathBuf,
    /// Where the instance keeps its sockets and logs.
    runtime_dir: PathBuf,
    /// The port the daemon serves on, if the configuration sets one.
    port: Option<u16>,
}

/// The `[docker]` settings the tasks read.
#[derive(Default, Deserialize)]
struct DockerConfig {
    /// The engine's socket, if the configuration names one.
    socket: Option<PathBuf>,
}

/// Read only to notice a configuration that serves on the removed shared
/// tailnet node.
#[derive(Default, Deserialize)]
struct TailscaleConfig {
    /// The tailnet node's socket, if the configuration names one.
    socket: Option<PathBuf>,
}

impl Config {
    /// Reads and parses the configuration at `path`.
    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
}

/// How far the supervisor got with the current sources.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    /// The daemon is building.
    Building,
    /// The build failed.
    Failed,
    /// The daemon runs.
    Running,
    /// The daemon exited.
    Exited,
}

impl Phase {
    /// Its name, as `status` prints it.
    fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Exited => "exited",
        }
    }
}

/// The supervisor's progress, in `state.json` for `wait` and `status`.
#[derive(Serialize, Deserialize)]
struct State {
    /// How far the supervisor got.
    phase: Phase,
    /// When the build of the current sources began. Sources modified later
    /// are not built yet.
    build_started: SystemTime,
}

/// This worktree's instance, as configured by piqueld.local.toml.
pub(crate) struct Instance {
    /// The worktree the instance belongs to.
    workspace: Workspace,
    /// The parsed configuration.
    config: Config,
}

impl Instance {
    /// Loads the instance configured in `workspace`'s piqueld.local.toml; fails when it is missing.
    fn load(workspace: Workspace) -> Result<Self> {
        let path = workspace.root().join(CONFIG_FILE);
        if !path.exists() {
            bail!("{CONFIG_FILE} is missing; write it with `just dev config`");
        }
        let config = Config::read(&path)?;
        let instance = Self { workspace, config };
        instance.own_runtime_dir()?;
        Ok(instance)
    }

    /// Creates the runtime directory unless it exists, and checks that it and
    /// its parent are the user's alone, before any task touches the sockets,
    /// logs, or dev.pid in it. Warns about settings older checkouts wrote,
    /// which `just dev config --force` replaces.
    fn own_runtime_dir(&self) -> Result<()> {
        let dir = &self.config.server.runtime_dir;
        let parent = dir.parent().context("runtime_dir has no parent")?;
        let replace = "run `just dev stop`, then `just dev config --force`";
        if parent == Path::new(LEGACY_RUNTIME_ROOT) {
            eprintln!(
                "warning: runtime_dir is in {LEGACY_RUNTIME_ROOT}, which other users can claim; \
                 {replace}"
            );
        }
        let socket = self.config.tailscale.socket.as_deref();
        if socket.is_some_and(|socket| socket.ends_with(LEGACY_TAILNET_SOCKET)) {
            eprintln!(
                "warning: {CONFIG_FILE} serves on the shared tailnet node, which `just dev` \
                 no longer starts; {replace}"
            );
        }
        Self::own_private_dir(parent.to_owned())?;
        Self::own_private_dir(dir.clone())?;
        Ok(())
    }

    /// Whether `workspace`'s own supervisor runs from `runtime_dir`. It would
    /// restart the daemon with a replaced configuration, out of reach of the
    /// tasks that read it. A configuration copied from another worktree names
    /// that worktree's instance instead, which is not ours to stop.
    /// Fails when the running supervisor's worktree is unknown.
    fn runs_from(runtime_dir: &Path, workspace: &Workspace) -> Result<bool> {
        match Supervisor::in_dir(runtime_dir).holder() {
            Holder::Nobody => Ok(false),
            Holder::Worktree(owner) => Ok(owner == workspace.root()),
            Holder::Unrecorded => bail!(
                "an instance that records no worktree, such as one started by an older xtask, \
                 is running from {}; stop it from the worktree that started it \
                 (`just dev stop`), then run `just dev config --force`",
                runtime_dir.display()
            ),
        }
    }

    /// Whether `dir` and its parent are directories only the current user can
    /// access, so that files in it, such as dev.pid, can be trusted.
    fn private_runtime_dir(dir: &Path) -> bool {
        dir.parent()
            .is_some_and(|parent| Self::check_private(parent).is_ok())
            && Self::check_private(dir).is_ok()
    }

    /// Like [`Self::load`], after writing piqueld.local.toml if it doesn't exist.
    fn load_or_configure(workspace: Workspace) -> Result<Self> {
        Self::configure(&workspace, false)?;
        Self::load(workspace)
    }

    /// Where instances keep their data. It stays out of /tmp, whose old files
    /// systemd may delete.
    fn state_root() -> Result<PathBuf> {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
            .context("HOME is not set")?;
        Ok(state_home.join("piqueld-dev"))
    }

    /// Holds every instance's sockets and logs. It is in /tmp because every
    /// session sees it, unlike /run/user, so it is named after the user and
    /// must be theirs alone: another user may have created it first.
    fn runtime_root() -> Result<PathBuf> {
        Self::own_private_dir(format!("/tmp/piqueld-dev-{}", getuid().as_raw()).into())
    }

    /// Creates `path` unless it exists, then checks it with [`Self::check_private`].
    fn own_private_dir(path: PathBuf) -> Result<PathBuf> {
        if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&path)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error).with_context(|| format!("create {}", path.display()));
        }
        Self::check_private(&path)?;
        Ok(path)
    }

    /// Checks that `path` is a directory, not a symbolic link, that only the
    /// current user can access.
    fn check_private(path: &Path) -> Result<()> {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == getuid().as_raw()
                && metadata.mode() & 0o777 == 0o700,
            "{} is not a directory only you can access; remove it",
            path.display()
        );
        Ok(())
    }

    /// Writes the worktree's configuration unless it exists and `force` is
    /// unset. Returns whether it did.
    fn configure(workspace: &Workspace, force: bool) -> Result<bool> {
        let path = workspace.root().join(CONFIG_FILE);
        if path.exists() {
            if !force {
                return Ok(false);
            }
            if let Ok(config) = Config::read(&path)
                && Self::runs_from(&config.server.runtime_dir, workspace)?
            {
                bail!(
                    "the instance is running; run `just dev stop`, then `just dev config --force`"
                );
            }
        }
        let name = Self::name_for(workspace)?;
        let data = Self::state_root()?.join(&name);
        let runtime = Self::runtime_root()?.join(&name);
        let _lock = Self::lock_ports()?;
        let port = Self::free_port(workspace)?;
        let quote = |path: &Path| toml::Value::from(path.display().to_string()).to_string();
        let (data, socket, runtime) = (
            quote(&data),
            quote(&runtime.join("docker/docker.sock")),
            quote(&runtime),
        );
        fs::write(
            &path,
            format!(
                r#"# Development instance for this worktree, generated by `just dev config`.
# `just dev` reads the directories, port, and Docker socket from here.
# Unset settings use the daemon's defaults; see examples/piqueld.toml.

[server]
data_dir = {data}
runtime_dir = {runtime}
# The browser opens public_url, through a port forward when it runs on
# another machine; the Unix socket serves the CLI.
listen_mode = "localhost"
port = {port}
# The instance's own host on localhost: browsers send a host's cookies to
# all of its ports.
allowed_hosts = ["{name}.localhost"]

[auth]
# The only origin where passkeys and browser sign-in work.
public_url = "http://{name}.localhost:{port}"

[docker]
# A private Docker-in-Docker engine, started by `just dev`. Point this at
# /var/run/docker.sock to use the host engine instead.
socket = {socket}
auto_initialize_swarm = true
"#
            ),
        )
        .with_context(|| format!("write {}", path.display()))?;
        eprintln!("wrote {CONFIG_FILE} for instance {name}");
        Ok(true)
    }

    /// The instance is named after the worktree directory, or `main` for the
    /// main checkout, followed by a hash of its path that keeps checkouts with
    /// the same name apart, e.g. `t3code-b681a751-0c1d2e3f`. The configuration
    /// records the name, so the hash need not be stable across Rust versions.
    fn name_for(workspace: &Workspace) -> Result<String> {
        let main = workspace.worktrees()?.into_iter().next();
        let name = if main.as_deref() == Some(workspace.root()) {
            "main".to_owned()
        } else {
            workspace
                .root()
                .file_name()
                .context("the worktree has no directory name")?
                .to_string_lossy()
                .to_lowercase()
        };
        let mut slug = String::new();
        for character in name.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character);
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        slug.truncate(40);
        let mut hasher = DefaultHasher::new();
        workspace.root().hash(&mut hasher);
        let hash = hasher.finish() & 0xffff_ffff;
        Ok(format!("{}-{hash:08x}", slug.trim_matches('-'))
            .trim_start_matches('-')
            .to_owned())
    }

    /// The lowest port from 7846 that nothing listens on and no other
    /// worktree's configuration claims.
    fn free_port(workspace: &Workspace) -> Result<u16> {
        let claimed: Vec<u16> = workspace
            .worktrees()?
            .iter()
            .filter(|worktree| worktree.as_path() != workspace.root())
            .filter_map(|worktree| Config::read(&worktree.join(CONFIG_FILE)).ok()?.server.port)
            .collect();
        (7846..=u16::MAX)
            .find(|port| {
                !claimed.contains(port) && std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok()
            })
            .context("no free port")
    }

    /// Locks the choice of ports across worktrees until the returned file is
    /// dropped, waiting for any other worktree's setup to finish.
    fn lock_ports() -> Result<fs::File> {
        let root = Self::state_root()?;
        Self::create_private_dir(&root)?;
        let path = root.join("ports.lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(file)
    }

    /// The path of `name` in the runtime directory.
    fn file(&self, name: &str) -> PathBuf {
        self.config.server.runtime_dir.join(name)
    }

    /// The daemon's socket.
    fn socket(&self) -> PathBuf {
        self.file("piqueld.sock")
    }

    /// The instance's name, that of its runtime directory.
    fn name(&self) -> String {
        self.config
            .server
            .runtime_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// The name of the instance's engine container.
    fn container(&self) -> String {
        format!("piqueld-dev-{}", self.name())
    }

    /// The socket of the instance's own engine.
    fn engine_socket(&self) -> PathBuf {
        self.file("docker/docker.sock")
    }

    /// Whether the configuration points at this instance's own engine.
    fn private_engine(&self) -> bool {
        self.config.docker.socket.as_deref() == Some(&self.engine_socket())
    }

    /// Whether the instance's own engine socket exists.
    fn engine_socket_exists(&self) -> bool {
        fs::metadata(self.engine_socket()).is_ok_and(|metadata| metadata.file_type().is_socket())
    }

    /// Creates `path` and its parents, accessible only to the current user.
    fn create_private_dir(path: &Path) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("create {}", path.display()))
    }

    /// This instance's supervisor.
    fn supervisor(&self) -> Supervisor {
        Supervisor::in_dir(&self.config.server.runtime_dir)
    }

    /// The supervisor's recorded state, if it has recorded a readable one.
    fn state(&self) -> Option<State> {
        serde_json::from_slice(&fs::read(self.file("state.json")).ok()?).ok()
    }

    /// Whether any source was modified after `time`.
    fn sources_changed_since(&self, time: SystemTime) -> bool {
        SOURCES
            .iter()
            .any(|source| Self::modified_since(&self.workspace.root().join(source), time))
    }

    /// Whether `path`, or anything visible below it, was modified after `time`.
    /// Directories count too, so deleted files are noticed.
    fn modified_since(path: &Path, time: SystemTime) -> bool {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        if metadata.modified().is_ok_and(|modified| modified > time) {
            return true;
        }
        metadata.is_dir()
            && fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    !entry.file_name().to_string_lossy().starts_with('.')
                        && Self::modified_since(&entry.path(), time)
                })
    }

    /// The daemon's public URL, once it serves the API. The authentication
    /// status is served before initial setup, unlike the socket's /health.
    async fn public_url(&self) -> Option<String> {
        let client = match piqueld_client::Client::unix(self.socket()) {
            Ok(client) => client.with_timeout(QUERY_TIMEOUT),
            Err(error) => {
                eprintln!(
                    "could not query the development instance: {:#}",
                    anyhow::Error::from(error)
                );
                return None;
            }
        };
        Some(client.auth_status().await.ok()?.public_url)
    }

    /// Prints the URL once the daemon serves the current sources. Fails with
    /// the relevant output when the build or startup fails.
    async fn wait(&self, timeout: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(timeout);
        while Instant::now() < deadline {
            if self.supervisor().running().is_none() {
                self.print_tail("dev.log", 40);
                bail!("the instance is not running; start it with `just dev start`");
            }
            if let Some(state) = self.state()
                && !self.sources_changed_since(state.build_started)
            {
                match state.phase {
                    Phase::Building => {}
                    Phase::Failed => {
                        self.print_tail("output.log", 60);
                        bail!("the build failed; fix it and save to rebuild");
                    }
                    Phase::Exited => {
                        self.print_tail("output.log", 60);
                        bail!("the daemon exited; fix it and save to rebuild");
                    }
                    Phase::Running => {
                        if let Some(url) = self.public_url().await {
                            println!("ready: {url}/dashboard/");
                            return Ok(());
                        }
                    }
                }
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
        bail!("not ready after {timeout} seconds; see `just dev status`")
    }

    /// Prints the last `count` lines of the runtime file `name` to stderr, if it exists.
    fn print_tail(&self, name: &str, count: usize) {
        let Ok(text) = fs::read_to_string(self.file(name)) else {
            return;
        };
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(count)..] {
            eprintln!("{line}");
        }
    }

    /// Prints the instance's name, state, URL and files.
    async fn status(&self) {
        let (phase, url) = if self.supervisor().running().is_some() {
            let phase = self
                .state()
                .map_or("starting", |state| state.phase.as_str());
            (phase, self.public_url().await)
        } else {
            ("stopped", None)
        };
        let docker = self.config.docker.socket.as_ref().map_or_else(
            || "the daemon's default".to_owned(),
            |socket| socket.display().to_string(),
        );
        println!("instance  {} ({phase})", self.name());
        println!(
            "url       {}",
            url.map_or_else(
                || "unavailable".to_owned(),
                |url| format!("{url}/dashboard/")
            )
        );
        println!("socket    {}", self.socket().display());
        println!("data      {}", self.config.server.data_dir.display());
        println!("docker    {docker}");
        println!("output    {}", self.file("output.log").display());
        println!("logs      {}", self.file("daemon.log").display());
    }

    /// Stops the instance and deletes its engine, data and runtime files.
    async fn clean(&self) -> Result<()> {
        let engine = Engine::host().await?;
        let container = self.container();
        let inspected = engine
            .inspect_container(&container, None::<InspectContainerOptions>)
            .await;
        if let Some(inspected) =
            Engine::found(inspected).context("inspect the instance's engine")?
        {
            self.check_owner(
                &container,
                inspected.config.and_then(|config| config.labels),
            )?;
        }
        self.supervisor().stop().await?;
        let dirs = [
            &self.config.server.data_dir,
            &self.config.server.runtime_dir,
        ];
        engine.remove_dev_instance(&container, &dirs).await?;
        eprintln!("removed instance {}", self.name());
        Ok(())
    }

    /// Fails unless `container`, with these labels, is this worktree's engine:
    /// a configuration copied from another worktree names that worktree's
    /// engine, and a container that `just dev` did not create is not ours to
    /// delete.
    fn check_owner(&self, container: &str, labels: Option<HashMap<String, String>>) -> Result<()> {
        match labels.unwrap_or_default().get(WORKTREE_LABEL) {
            Some(owner) if Path::new(owner) == self.workspace.root() => Ok(()),
            Some(owner) => {
                bail!("{container} belongs to {owner}; run `just dev config --force` here")
            }
            None => bail!("{container} was not created by `just dev`; kept it"),
        }
    }

    /// Replaces xtask with this worktree's piquelctl; returns only on failure.
    /// Cargo's output stays visible: the build lock is shared with the dev
    /// daemon's rebuilds, so a wait for it would otherwise look like a hang.
    fn ctl(&self, args: &[String]) -> anyhow::Error {
        let error = self
            .workspace
            .cargo()
            .args(["run", "--package", "piquelctl", "--"])
            .args(args)
            .env("PIQUELD_SOCKET", self.socket())
            .exec();
        anyhow::Error::new(error).context("run piquelctl")
    }
}

impl Engine {
    /// Removes an instance's engine, its image store, and `dirs`. Only
    /// directories directly under a `piqueld-dev` directory, such as the state
    /// root, or under the runtime root are deleted, so a configuration pointing
    /// elsewhere never loses data.
    async fn remove_dev_instance(&self, container: &str, dirs: &[impl AsRef<Path>]) -> Result<()> {
        self.remove(container).await?;
        Self::found(
            self.remove_volume(container, None::<RemoveVolumeOptions>)
                .await,
        )
        .context("remove the instance's image store")?;
        let runtime_root =
            fs::canonicalize(Instance::runtime_root()?).context("resolve the runtime root")?;
        for dir in dirs {
            let Ok(dir) = fs::canonicalize(dir) else {
                continue;
            };
            let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
                continue;
            };
            if parent.file_name() != Some("piqueld-dev".as_ref()) && parent != runtime_root {
                eprintln!("kept {}", dir.display());
            } else if fs::remove_dir_all(&dir).is_err() && dir.exists() {
                self.run_once(ContainerCreateBody {
                    image: Some(Self::dind_image()),
                    entrypoint: Some(vec!["rm".to_owned()]),
                    cmd: Some(vec![
                        "-rf".to_owned(),
                        format!("/parent/{}", name.display()),
                    ]),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!("{}:/parent", parent.display())]),
                        ..HostConfig::default()
                    }),
                    ..ContainerCreateBody::default()
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Stops and cleans the instances of worktrees that no longer exist.
    async fn prune_dev_instances(&self) -> Result<()> {
        let filters = HashMap::from([("label", vec![WORKTREE_LABEL])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        for container in self.list_containers(Some(options)).await? {
            let labels = container.labels.unwrap_or_default();
            let name = container.names.unwrap_or_default().into_iter().next();
            let (Some(name), Some(worktree)) = (name, labels.get(WORKTREE_LABEL)) else {
                continue;
            };
            if Path::new(worktree).is_dir() {
                continue;
            }
            if let Some(runtime) = labels.get(RUNTIME_LABEL).map(Path::new)
                && Instance::private_runtime_dir(runtime)
            {
                Supervisor::in_dir(runtime).stop().await?;
            }
            let dirs: Vec<&String> = [DATA_LABEL, RUNTIME_LABEL]
                .iter()
                .filter_map(|label| labels.get(*label))
                .collect();
            self.remove_dev_instance(name.trim_start_matches('/'), &dirs)
                .await?;
            eprintln!("removed the instance of {worktree}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(root: &Path) -> Instance {
        instance_in(root, Path::new("/runtime"))
    }

    fn instance_in(root: &Path, runtime_dir: &Path) -> Instance {
        let config = format!(
            "[server]\ndata_dir = \"/data\"\nruntime_dir = {}",
            toml::Value::from(runtime_dir.display().to_string())
        );
        Instance {
            workspace: Workspace {
                root: root.to_owned(),
            },
            config: toml::from_str(&config).expect("the config parses"),
        }
    }

    #[test]
    fn clean_only_removes_this_worktrees_engine() {
        let instance = instance(Path::new("/worktrees/a"));
        let owned = |labels| instance.check_owner("engine", labels).is_ok();
        let labelled = |worktree: &str| {
            Some(HashMap::from([(
                WORKTREE_LABEL.to_owned(),
                worktree.to_owned(),
            )]))
        };
        assert!(owned(labelled("/worktrees/a")));
        assert!(!owned(labelled("/worktrees/b")));
        assert!(!owned(Some(HashMap::new())));
        assert!(!owned(None));
    }

    #[test]
    fn runtime_root_is_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().expect("the temp dir is created");
        let root = parent.path().join("root");
        Instance::own_private_dir(root.clone())
            .expect("`Instance::own_private_dir` should succeed");
        assert_eq!(
            fs::metadata(&root).expect("the metadata is read").mode() & 0o777,
            0o700
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
            .expect("the permissions are set");
        Instance::own_private_dir(root)
            .expect_err("`Instance::own_private_dir` should be rejected");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(parent.path(), &link).expect("the symlink is created");
        Instance::own_private_dir(link)
            .expect_err("`Instance::own_private_dir` should be rejected");
    }

    #[tokio::test]
    async fn config_force_only_waits_for_this_worktrees_instance() {
        let temporary = tempfile::tempdir().expect("the temp dir is created");
        let root = Instance::own_private_dir(temporary.path().join("root"))
            .expect("the private dir is owned");
        let runtime = Instance::own_private_dir(root.join("rt")).expect("the private dir is owned");
        let mine = instance_in(Path::new("/worktrees/a"), &runtime);
        let runs_from = |root: &str| {
            let workspace = Workspace { root: root.into() };
            Instance::runs_from(&runtime, &workspace)
                .expect("the instance runs from the runtime dir")
        };
        assert!(!runs_from("/worktrees/a"));
        let claim = mine.claim().await.expect("the claim is taken");
        assert!(runs_from("/worktrees/a"));
        assert!(!runs_from("/worktrees/b"));
        drop(claim);
        let _claim = Supervisor::in_dir(&runtime)
            .claim()
            .await
            .expect("the claim is taken");
        let workspace = Workspace {
            root: "/worktrees/a".into(),
        };
        Instance::runs_from(&runtime, &workspace)
            .expect_err("`Instance::runs_from` should be rejected");
    }

    #[test]
    fn migrations_and_configuration_are_sources() {
        let root = tempfile::tempdir().expect("the temp dir is created");
        fs::create_dir_all(root.path().join("crates/piqueld-store/migrations"))
            .expect("the directory is created");
        let instance = instance(root.path());
        let built = SystemTime::now() + Duration::from_hours(1);
        let change = |path: &str, modified: SystemTime| {
            let file = fs::File::create(root.path().join(path)).expect("the file is created");
            file.set_modified(modified)
                .expect("the modified time is set");
        };
        assert!(!instance.sources_changed_since(built));
        change(
            "crates/piqueld-store/migrations/0001_new.sql",
            built + Duration::from_secs(1),
        );
        assert!(instance.sources_changed_since(built));
        let built = built + Duration::from_secs(2);
        assert!(!instance.sources_changed_since(built));
        change(CONFIG_FILE, built + Duration::from_secs(1));
        assert!(instance.sources_changed_since(built));
    }
}
