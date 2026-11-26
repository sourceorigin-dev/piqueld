//! Isolated, bounded Git checkouts (see [`Checkouts`]) and branch listings,
//! and the snapshots of local worktrees deployments check out instead (see
//! [`snapshot`]). No shared mutable repository or credential store.

pub(crate) mod snapshot;

use anyhow::{Context, bail};
use piqueld_core::api::BranchState;
use piqueld_core::manifest::{GitRepository, valid_repository_path};
use piqueld_core::wait;
use piqueld_core::{GitCommit, ValidationErrors};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{collections::BTreeMap, time::Duration};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Where deployments get their Git sources: the checkouts they read
/// manifests from and build in, and the snapshots uploaded from local
/// worktrees, in the daemon's data directory. One per daemon, shared by the
/// API service, which receives snapshots, and the controller.
pub struct Sources {
    /// Admission for every Git checkout, made in `checkouts/`.
    checkouts: Arc<Checkouts>,
    /// Snapshots uploaded from local worktrees, kept in `snapshots/`.
    snapshots: snapshot::Snapshots,
    /// Largest snapshot bundle an upload may carry (`[snapshots]`).
    max_upload_bytes: std::num::NonZeroU64,
}

impl Sources {
    /// The data directory's entry holding Git checkouts, a cache of the
    /// repositories applications build from.
    pub const CHECKOUTS: &str = "checkouts";
    /// The data directory's entry holding the snapshots of local changes that
    /// previews deploy.
    pub const SNAPSHOTS: &str = "snapshots";

    /// Opens `checkouts/` and `snapshots/` in `directory` with the default
    /// limits, removing what a previous process left in progress.
    ///
    /// # Errors
    /// Returns the file system error preparing either directory.
    pub async fn open(directory: &Path) -> std::io::Result<Self> {
        Ok(Self {
            checkouts: Arc::new(Checkouts::open(directory.join(Self::CHECKOUTS)).await?),
            snapshots: snapshot::Snapshots::open(directory.join(Self::SNAPSHOTS)).await?,
            max_upload_bytes: piqueld_config::SnapshotsConfig::default().max_upload_bytes,
        })
    }

    /// Applies the daemon's `[checkouts] max_bytes` and `[snapshots]
    /// max_upload_bytes`, before any checkout is made.
    #[must_use]
    pub fn configured(self, config: &piqueld_config::DaemonConfig) -> Self {
        Self {
            checkouts: Arc::new(self.checkouts.with_max_bytes(config.checkouts.max_bytes)),
            max_upload_bytes: config.snapshots.max_upload_bytes,
            ..self
        }
    }

    /// Admission for every Git checkout.
    pub(crate) const fn checkouts(&self) -> &Arc<Checkouts> {
        &self.checkouts
    }

    /// Snapshots uploaded from local worktrees.
    pub(crate) const fn snapshots(&self) -> &snapshot::Snapshots {
        &self.snapshots
    }

    /// The largest local snapshot bundle an upload may carry, in bytes.
    pub(crate) const fn max_upload_bytes(&self) -> u64 {
        self.max_upload_bytes.get()
    }

    /// Removes the snapshots no preview's latest deployment uses, finding
    /// those that are in `store` while no upload is being accepted.
    ///
    /// # Errors
    /// Returns a storage or file system error.
    pub(crate) async fn prune_snapshots(
        &self,
        store: &piqueld_store::Store,
    ) -> Result<(), piqueld_store::StoreError> {
        self.snapshots.prune(store.local_snapshot_commits()).await
    }
}

/// The branch heads of a remote repository, listed with `git ls-remote`.
pub(crate) struct Heads {
    /// The repository listed.
    url: String,
    /// Head commit of each branch.
    branches: BTreeMap<String, String>,
}

impl Heads {
    /// Longest a listing may take before it counts as failed, well within a
    /// client's 30-second request budget so it still gets `unknown` branch
    /// states. Its process group, Git's helpers included, is then killed.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// Most output a listing may produce: tens of thousands of branches.
    const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

    /// Lists every branch of `url` with its head commit. Any repository,
    /// network, or authentication failure is an error, never an empty
    /// listing, so a branch only counts as gone when the repository answered.
    pub(crate) async fn list(url: &str) -> anyhow::Result<Self> {
        let mut command = Checkout::command();
        command.args(["ls-remote", "--heads", "--"]).arg(url);
        let output = tokio::time::timeout(
            Self::TIMEOUT,
            piqueld_docker::command::LoggedCommand::output(
                &mut command,
                "list Git branches",
                Self::OUTPUT_LIMIT,
            ),
        )
        .await
        .context("git ls-remote timed out")?
        .map_err(|error| {
            tracing::warn!(?error, "git ls-remote failed");
            match error.failure() {
                Some(failure) => anyhow::anyhow!("git ls-remote failed ({})", failure.status),
                None => anyhow::anyhow!("git ls-remote could not run"),
            }
        })?;
        let mut heads = BTreeMap::new();
        for line in String::from_utf8(output)
            .context("decode git ls-remote output")?
            .lines()
        {
            let (commit, reference) = line
                .split_once('\t')
                .context("parse git ls-remote output")?;
            if let Some(branch) = reference.strip_prefix("refs/heads/") {
                heads.insert(branch.to_owned(), commit.to_owned());
            }
        }
        Ok(Self {
            url: url.to_owned(),
            branches: heads,
        })
    }

    /// Heads listed some other way, for tests.
    #[cfg(test)]
    pub(crate) fn new(url: &str, branches: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            url: url.to_owned(),
            branches: branches.into_iter().collect(),
        }
    }

    /// The repository listed, which a branch state only describes.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The commit at `branch`'s head, unless the repository has no such branch.
    pub(crate) fn head(&self, branch: &str) -> Option<&str> {
        self.branches.get(branch).map(String::as_str)
    }

    /// Where `branch` is compared with `fetched`, the commit a preview last
    /// fetched its manifest from.
    pub(crate) fn state(&self, branch: &str, fetched: Option<&str>) -> BranchState {
        match (self.head(branch), fetched) {
            (None, _) => BranchState::Gone,
            (Some(head), Some(deployed)) if head != deployed => BranchState::Moved {
                head: head.to_owned(),
                deployed: deployed.to_owned(),
            },
            (Some(head), _) => BranchState::Exists {
                head: head.to_owned(),
            },
        }
    }
}

/// Branch listings for reads: one `git ls-remote` per repository at a
/// time, shared by every read that arrived while it ran, and at most
/// [`Self::LISTING`] at once, so frequent reads never fork a process each.
/// A listing is never reused once a later read arrives, so it is never
/// staler than the read itself, and is dropped once no read waits for it.
pub(crate) struct Listings {
    /// Each repository being listed, with its last listing and when it
    /// finished.
    repositories: parking_lot::Mutex<BTreeMap<String, Arc<Repository>>>,
    /// Permits to list.
    listing: Semaphore,
}

/// A finished listing.
type Listed = (std::time::Instant, Arc<anyhow::Result<Heads>>);

impl Default for Listings {
    fn default() -> Self {
        Self {
            repositories: parking_lot::Mutex::default(),
            listing: Semaphore::new(Self::LISTING),
        }
    }
}

impl Listings {
    /// Repositories listed at once.
    const LISTING: usize = 4;

    /// Lists `url`'s branches (see [`Heads::list`]), or shares the listing
    /// of a read that was already listing them.
    pub(crate) async fn list(&self, url: &str) -> Arc<anyhow::Result<Heads>> {
        let requested = std::time::Instant::now();
        let reader = Reader {
            listings: self,
            url,
            repository: Some(Arc::clone(
                self.repositories.lock().entry(url.to_owned()).or_default(),
            )),
        };
        let mut last = reader.repository().lock().await;
        if let Some((finished, heads)) = &*last
            && *finished > requested
        {
            return Arc::clone(heads);
        }
        let heads = match self.listing.acquire().await {
            Ok(_listing) => Heads::list(url).await,
            Err(error) => Err(error.into()),
        };
        let heads = Arc::new(heads);
        *last = Some((std::time::Instant::now(), Arc::clone(&heads)));
        heads
    }
}

/// A repository's last listing, and when it finished.
type Repository = tokio::sync::Mutex<Option<Listed>>;

/// A read of [`Listings`], which removes its repository's entry when it
/// is the last to finish, or is cancelled.
struct Reader<'a> {
    /// The listings the read belongs to.
    listings: &'a Listings,
    /// URL of the repository read.
    url: &'a str,
    /// Taken only when dropped.
    repository: Option<Arc<Repository>>,
}

impl Reader<'_> {
    /// The repository being read.
    #[expect(clippy::expect_used, reason = "Only `Drop` takes the repository.")]
    fn repository(&self) -> &Repository {
        self.repository
            .as_deref()
            .expect("taken only when the reader is dropped")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let mut repositories = self.listings.repositories.lock();
        if let Some(repository) = self.repository.take()
            && Arc::strong_count(&repository) == 2
        {
            drop(repository);
            repositories.remove(self.url);
        }
    }
}

/// Admission for Git checkouts, which every clone goes through: at most
/// [`Self::CLONING`] clones at once, and the files of every checkout held
/// at once, history and working tree, within `[checkouts] max_bytes`. A
/// clone is stopped once it downloads more than its share of the budget
/// (see [`Self::bounded`]); its tree is then measured before its files are
/// written. A checkout waits until it fits, and one that never could, or has
/// more than [`Self::FILES`] files, is refused. Clones in progress are not
/// counted, so checkouts take at most twice `max_bytes`.
pub(crate) struct Checkouts {
    /// Where checkouts are made: `<data_dir>/checkouts`, emptied at startup.
    root: PathBuf,
    /// Permits to clone, held until the clone is checked out, even while it
    /// waits for `disk`: its download isn't counted there yet, so these
    /// bound what waiting clones take.
    cloning: Semaphore,
    /// One permit per MiB of history and working tree, held by each
    /// [`Checkout`].
    disk: Arc<Semaphore>,
    /// The most bytes all checkouts may take at once.
    max_bytes: u64,
}

/// A checkout, or a download, larger than [`Checkouts`] admits.
#[derive(Debug, thiserror::Error)]
#[error("it takes more than {limit} bytes, or has more than {files} files", files = Checkouts::FILES)]
pub(crate) struct TreeTooLarge {
    /// `[checkouts] max_bytes`.
    pub(crate) limit: u64,
}

impl Checkouts {
    /// Clones at once, each downloading at most this share of `max_bytes`.
    const CLONING: usize = 4;
    /// How often a download's size is checked.
    const WATCH: Duration = Duration::from_millis(100);
    /// Applied before checking out, overriding the repository's own
    /// attributes so files are checked out as committed: no filter driver,
    /// encoding, line-ending, or `ident` conversion runs or grows them.
    const ATTRIBUTES: &str = "* -text -filter -working-tree-encoding -ident\n";
    /// Most files one working tree may have.
    pub(crate) const FILES: usize = 500_000;
    /// Bytes per permit of `disk`.
    const MIB: u64 = 1024 * 1024;

    /// Opens `root`, removing checkouts a previous process left behind, with
    /// the default `[checkouts] max_bytes`.
    pub(crate) async fn open(root: PathBuf) -> std::io::Result<Self> {
        Self::recreate(&root).await?;
        Ok(Self::new(
            root,
            piqueld_config::CheckoutsConfig::default().max_bytes,
        ))
    }

    /// Empties `directory`, removing whatever a previous process left in it,
    /// and creates it, with its parents, when missing.
    pub(crate) async fn recreate(directory: &Path) -> std::io::Result<()> {
        match tokio::fs::remove_dir_all(directory).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        tokio::fs::create_dir_all(directory).await
    }

    /// The same checkouts directory within `max_bytes` instead, before any
    /// checkout is made.
    #[must_use]
    pub(crate) fn with_max_bytes(&self, max_bytes: std::num::NonZeroU64) -> Self {
        Self::new(self.root.clone(), max_bytes)
    }

    /// Checkouts in `root` within `max_bytes`.
    fn new(root: PathBuf, max_bytes: std::num::NonZeroU64) -> Self {
        let max_bytes = max_bytes.get();
        let permits = usize::try_from(max_bytes.div_ceil(Self::MIB))
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        Self {
            root,
            cloning: Semaphore::new(Self::CLONING),
            disk: Arc::new(Semaphore::new(permits)),
            max_bytes,
        }
    }

    /// The most bytes all checkouts may take at once.
    pub(crate) const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The refusal of a checkout or download larger than `max_bytes`.
    const fn too_large(&self) -> TreeTooLarge {
        TreeTooLarge {
            limit: self.max_bytes,
        }
    }

    /// Clones the repository shallowly into a new directory and checks out
    /// a detached commit, as committed, once it fits, streaming Git output
    /// into `log` when supplied.
    ///
    /// A pinned `commit` is fetched explicitly (the branch may no longer
    /// exist), with the default branch's whole history instead when the
    /// server serves no unadvertised commit; otherwise the tip of `branch`
    /// is used. The resolved `HEAD` must be a full commit hash.
    ///
    /// # Errors
    /// Returns a Git failure, or [`TreeTooLarge`].
    pub(crate) async fn checkout(
        &self,
        repository: &GitRepository,
        log: Option<&dyn piqueld_docker::command::OutputLog>,
    ) -> anyhow::Result<Checkout> {
        let mut errors = Vec::new();
        repository.validate("repository", &mut errors);
        if !errors.is_empty() {
            return Err(anyhow::Error::new(ValidationErrors(errors))
                .context("invalid Git repository configuration"));
        }
        let _cloning = self.cloning.acquire().await?;
        let directory = tempfile::Builder::new()
            .prefix("checkout-")
            .tempdir_in(&self.root)
            .context("create Git checkout directory")?;
        let root = directory.path().join("repository");
        let download = Box::pin(Self::download(repository, &root, log));
        let share = self.max_bytes / Self::CLONING as u64;
        Self::bounded(directory.path(), share, download).await?;
        let revision = repository.commit.as_deref().unwrap_or("HEAD");
        let tree = self.measure(&root, revision).await?;
        let bytes = tree.saturating_add(Self::disk_usage(directory.path()).await?);
        if bytes > self.max_bytes {
            return Err(self.too_large().into());
        }
        let disk = Arc::clone(&self.disk)
            .acquire_many_owned(Self::permits(bytes)?)
            .await?;
        let info = root.join(".git/info");
        tokio::fs::create_dir_all(&info)
            .await
            .context("create Git info directory")?;
        tokio::fs::write(info.join("attributes"), Self::ATTRIBUTES)
            .await
            .context("write Git checkout attributes")?;
        piqueld_docker::command::LoggedCommand::run_recorded(
            Checkout::command_in(&root).args(["checkout", "--detach", revision, "--"]),
            "checkout Git commit",
            log,
        )
        .await?;
        let commit = Checkout::head(&root).await?;
        let disk = self.settle(directory.path(), disk).await?;
        let checkout = Checkout {
            directory,
            _disk: disk,
            commit,
        };
        checkout.restore_modes().await?;
        Ok(checkout)
    }

    /// Checks the disk a checkout in `directory` takes against `disk`, its
    /// reservation, which an estimate may fall short of: it is topped up
    /// without waiting, or the checkout is refused.
    #[expect(
        clippy::map_err_ignore,
        reason = "the only failure is no free permit, which `TreeTooLarge` names"
    )]
    async fn settle(
        &self,
        directory: &Path,
        mut disk: OwnedSemaphorePermit,
    ) -> anyhow::Result<OwnedSemaphorePermit> {
        let needed = Self::permits(Self::disk_usage(directory).await?)?;
        let held =
            u32::try_from(disk.num_permits()).context("count the checkout's disk permits")?;
        if let Some(more) = needed.checked_sub(held).filter(|more| *more > 0) {
            let more = Arc::clone(&self.disk)
                .try_acquire_many_owned(more)
                .map_err(|_| self.too_large())?;
            disk.merge(more);
        }
        Ok(disk)
    }

    /// The permits of `disk` that `bytes` take.
    fn permits(bytes: u64) -> anyhow::Result<u32> {
        u32::try_from(bytes.div_ceil(Self::MIB).max(1)).context("count the checkout's disk permits")
    }

    /// The bytes `revision`'s working tree takes in the repository at
    /// `repository`, each file and directory charged
    /// `SnapshotLimits::file_cost`, read from object headers without
    /// writing a file.
    ///
    /// # Errors
    /// Returns a Git failure, or [`TreeTooLarge`] when it takes more than
    /// `[checkouts] max_bytes` or has more than [`Self::FILES`] files.
    pub(crate) async fn measure(&self, repository: &Path, revision: &str) -> anyhow::Result<u64> {
        let mut command = Checkout::command_in(repository);
        command.args([
            "ls-tree",
            "-r",
            "-t",
            "--format=%(objecttype) %(objectsize)",
            "--end-of-options",
            revision,
        ]);
        let output = piqueld_docker::command::LoggedCommand::output(
            &mut command,
            "measure Git tree",
            Self::FILES * 28,
        )
        .await
        .map_err(|error| match error.limit() {
            Some(_) => self.too_large().into(),
            None => anyhow::Error::from(error),
        })?;
        let output = String::from_utf8(output).context("decode Git tree sizes")?;
        let mut bytes = 0_u64;
        for (files, entry) in output.lines().enumerate() {
            let size = match entry.split_once(' ') {
                Some(("blob", size)) => size.parse().context("parse a Git blob size")?,
                _ => 0,
            };
            let cost = piqueld_core::api::SnapshotLimits::file_cost(size);
            bytes = bytes.saturating_add(cost);
            if files >= Self::FILES || bytes > self.max_bytes {
                return Err(self.too_large().into());
            }
        }
        Ok(bytes)
    }

    /// Clones `repository` shallowly into `root` without checking it out,
    /// and fetches its pinned commit, if any (see [`Self::checkout`]).
    async fn download(
        repository: &GitRepository,
        root: &Path,
        log: Option<&dyn piqueld_docker::command::OutputLog>,
    ) -> anyhow::Result<()> {
        let mut clone = Checkout::command();
        clone.args([
            "clone",
            "--no-checkout",
            "--single-branch",
            "--depth=1",
            "--no-local",
        ]);
        if repository.commit.is_none() {
            clone.args(["--branch", &repository.branch]);
        }
        clone.arg("--").arg(&repository.url).arg(root);
        piqueld_docker::command::LoggedCommand::run_recorded(
            &mut clone,
            "clone Git repository",
            log,
        )
        .await?;
        let Some(commit) = &repository.commit else {
            return Ok(());
        };
        let fetch = |arguments: &[&str]| {
            let mut fetch = Checkout::command_in(root);
            fetch.arg("fetch").args(arguments);
            fetch
        };
        let pinned = piqueld_docker::command::LoggedCommand::run_recorded(
            &mut fetch(&["--depth=1", "origin", commit]),
            "fetch pinned Git commit",
            log,
        )
        .await;
        if pinned.is_err() {
            piqueld_docker::command::LoggedCommand::run_recorded(
                &mut fetch(&[
                    "--unshallow",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                ]),
                "fetch Git history",
                log,
            )
            .await?;
        }
        Ok(())
    }

    /// Runs `transfer`, which downloads into `directory`, and stops it,
    /// with its process group, once the directory takes more than `limit`
    /// bytes; one that finishes larger fails too.
    ///
    /// # Errors
    /// Returns `transfer`'s error, or [`TreeTooLarge`].
    pub(crate) async fn bounded<T, E: From<TreeTooLarge>>(
        directory: &Path,
        limit: u64,
        transfer: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let watch = async {
            loop {
                wait::pause(Self::WATCH).await;
                if Self::measured(directory)
                    .await
                    .is_some_and(|bytes| bytes > limit)
                {
                    return TreeTooLarge { limit };
                }
            }
        };
        let transferred = tokio::select! {
            result = transfer => result?,
            too_large = watch => return Err(too_large.into()),
        };
        match Self::measured(directory).await {
            Some(bytes) if bytes <= limit => Ok(transferred),
            _ => Err(TreeTooLarge { limit }.into()),
        }
    }

    /// The [disk usage](Self::disk_usage) of a download in `directory`, or
    /// none, logged, when it can't be measured.
    async fn measured(directory: &Path) -> Option<u64> {
        Self::disk_usage(directory)
            .await
            .inspect_err(|error| tracing::warn!(?error, "measuring a Git download failed"))
            .ok()
    }

    /// The disk allocated to `directory` and everything under it, which
    /// Git may be writing.
    async fn disk_usage(directory: &Path) -> anyhow::Result<u64> {
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::MetadataExt;
            let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
            let (mut bytes, mut directories) = (0_u64, vec![directory]);
            while let Some(directory) = directories.pop() {
                let entries = match std::fs::read_dir(&directory) {
                    Err(error) if gone(&error) => continue,
                    entries => entries?,
                };
                for entry in entries {
                    let metadata =
                        match entry.and_then(|entry| Ok((entry.path(), entry.metadata()?))) {
                            Err(error) if gone(&error) => continue,
                            metadata => metadata?,
                        };
                    let (path, metadata) = metadata;
                    bytes = bytes.saturating_add(metadata.blocks().saturating_mul(512));
                    if metadata.is_dir() {
                        directories.push(path);
                    }
                }
            }
            Ok(bytes)
        })
        .await
        .context("measure Git checkout")?
    }
}

/// A private checkout pinned once, retained until preparation finishes.
pub(crate) struct Checkout {
    /// Temporary directory removed when the checkout is dropped.
    directory: tempfile::TempDir,
    /// Its share of [`Checkouts`]' disk, released once the directory is gone.
    _disk: OwnedSemaphorePermit,
    /// Full commit hash actually checked out.
    pub(crate) commit: GitCommit,
}

impl Checkout {
    /// Builds the checkout's image, labelled for `owner` and queued with
    /// `priority`, streaming Docker's output into `log` when supplied.
    /// Returns the built image.
    pub(crate) async fn build(
        &self,
        build: &piqueld_core::manifest::ValidatedBuild,
        (owner, priority): (&piqueld_core::InstanceId, piqueld_docker::BuildPriority),
        docker: &impl piqueld_docker::DockerApi,
        log: Option<&dyn piqueld_docker::command::OutputLog>,
    ) -> anyhow::Result<piqueld_core::BuiltImage> {
        let piqueld_core::manifest::ValidatedBuild::Docker {
            dockerfile,
            context,
            args,
            target,
        } = build;
        let build = piqueld_docker::ImageBuild {
            owner,
            dockerfile: self.path(dockerfile).await?,
            context: self.path(context).await?,
            args,
            target: target.as_deref(),
            priority,
        };
        docker
            .build_image(&build, log)
            .await
            .context("build Git source image")
    }

    /// Most memory one Git allocation may take, such as an object being
    /// inflated or resolved from its deltas: a small pack can describe huge
    /// objects. Larger files are streamed instead (`core.bigFileThreshold`).
    const ALLOC_LIMIT: &str = "1g";

    /// Builds a hardened `git` command: restricted transports, no interactive
    /// prompts, hooks disabled, system configuration ignored, allocations within
    /// [`Self::ALLOC_LIMIT`], and killed if the future is dropped.
    fn command() -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ALLOC_LIMIT", Self::ALLOC_LIMIT);
        command.args(["-c", "core.hooksPath=/dev/null"]);
        command.kill_on_drop(true);
        command
    }

    /// A hardened `git` command (see [`Self::command`]) run in the
    /// repository at `directory`.
    pub(crate) fn command_in(directory: &Path) -> Command {
        let mut command = Self::command();
        command.arg("-C").arg(directory);
        command
    }

    /// The full commit hash `HEAD` resolves to in the repository at `root`.
    async fn head(root: &Path) -> anyhow::Result<GitCommit> {
        let output = Self::command_in(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .context("resolve Git commit")?;
        if !output.status.success() {
            bail!("Git did not resolve a full commit hash");
        }
        let commit = String::from_utf8(output.stdout).context("decode Git commit")?;
        GitCommit::parse(commit.trim()).context("Git did not resolve a full commit hash")
    }

    /// Gives the working tree the modes Git records, whatever the daemon's
    /// umask (`UMask=0077` in the NixOS module checks files out `0600`):
    /// directories and executables `0755`, other files `0644`. Images copy
    /// these modes, so a service running as an unprivileged user can read
    /// the files it was built with. The checkout's temporary directory stays
    /// private, and so does `.git`; symlinks are left alone.
    async fn restore_modes(&self) -> anyhow::Result<()> {
        let root = self.root();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            let set = |path: &Path, mode| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .with_context(|| format!("set the mode of {}", path.display()))
            };
            let mut directories = vec![root.clone()];
            while let Some(directory) = directories.pop() {
                set(&directory, 0o755)?;
                let entries = std::fs::read_dir(&directory)
                    .with_context(|| format!("read {}", directory.display()))?;
                for entry in entries {
                    let entry = entry.with_context(|| format!("read {}", directory.display()))?;
                    let path = entry.path();
                    let metadata = entry
                        .metadata()
                        .with_context(|| format!("read the mode of {}", path.display()))?;
                    if metadata.is_dir() && path != root.join(".git") {
                        directories.push(path);
                    } else if metadata.is_file() {
                        let executable = metadata.permissions().mode() & 0o100 != 0;
                        set(&path, if executable { 0o755 } else { 0o644 })?;
                    }
                }
            }
            anyhow::Ok(())
        })
        .await
        .context("join the Git checkout mode restoration")?
        .context("restore Git checkout modes")
    }

    /// Working tree root inside the temporary directory.
    pub(crate) fn root(&self) -> PathBuf {
        self.directory.path().join("repository")
    }

    /// Resolves a manifest-relative path to an existing canonical path inside the
    /// working tree. Symlinks are followed, so anything escaping the checkout or
    /// pointing into `.git` is rejected.
    ///
    /// ```text
    /// "./Dockerfile"       -> <checkout>/repository/Dockerfile
    /// "../Dockerfile"      -> error (leaves the repository)
    /// "link-to-etc/passwd" -> error (symlink escapes the checkout)
    /// ```
    pub(crate) async fn path(&self, relative: &str) -> anyhow::Result<PathBuf> {
        if !valid_repository_path(relative) {
            bail!("path must remain within the repository root");
        }
        let root = tokio::fs::canonicalize(self.root())
            .await
            .context("locate checkout root")?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .context("locate repository file")?;
        if !path.starts_with(&root) || path.starts_with(root.join(".git")) {
            bail!("repository path escapes the checkout");
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests;
