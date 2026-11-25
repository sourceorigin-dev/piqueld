//! Snapshots of the current Git worktree for `preview deploy --local`.
//!
//! A snapshot is a commit of the worktree's tracked and untracked files on
//! top of `HEAD`, uploaded as a Git bundle. Ignored files are left out, or
//! kept as committed when tracked. It never changes the user's repository:
//! Git writes the snapshot's index, objects, and ref into a private
//! temporary directory instead. Its files are measured against the daemon's
//! limits before anything is written (see [`Worktree::snapshot`]); the
//! bundle is thin against a base the application's repository has, or holds
//! the whole history only when asked to (see [`History`]).
use crate::error::{CliError, ErrorKind, Result};
use piqueld_client::{LocalSnapshot, SnapshotLimits, observability::Bytes, sync::short_commit};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Output, Stdio},
};
use tokio::process::Command;

/// Variables that would point Git commands at another repository, index,
/// or object directory than the ones each command names.
const REPOSITORY_VARIABLES: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// The Git worktree containing the current directory.
pub(crate) struct Worktree {
    /// Its top-level directory.
    root: PathBuf,
    /// Its index file.
    index: PathBuf,
    /// The repository's object directory, shared by its worktrees.
    objects: PathBuf,
}

/// A snapshot commit, written into its own temporary directory.
pub(crate) struct Snapshot<'a> {
    worktree: &'a Worktree,
    /// Holds the snapshot's index, objects, bare repository, and bundles.
    directory: tempfile::TempDir,
    /// The snapshot commit.
    pub(crate) commit: String,
    /// The files it adds, changes, or removes compared with `HEAD`.
    pub(crate) uncommitted: Vec<String>,
}

/// What a bundle carries besides the snapshot commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum History {
    /// The commits since the nearest ancestor of `HEAD` that a
    /// remote-tracking branch has, which the daemon fetches from the
    /// application's repository. Refused when there is none.
    Thin,
    /// Every commit of `HEAD`, which the daemon needs nothing to apply.
    Full,
}

/// A bundle of a snapshot, ready to upload.
pub(crate) struct Bundle {
    pub(crate) path: PathBuf,
    /// Its size in bytes.
    pub(crate) length: u64,
    /// The commit it carries and the base it needs.
    pub(crate) snapshot: LocalSnapshot,
    /// Commits of `HEAD` it carries besides the snapshot commit.
    commits: u64,
}

impl Worktree {
    /// Finds the worktree containing the current directory.
    pub(crate) async fn current() -> Result<Self> {
        Self::at(Path::new(".")).await
    }

    /// Finds the worktree containing `directory`.
    pub(crate) async fn at(directory: &Path) -> Result<Self> {
        let output = Git::run(
            Git::command(directory).args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-path",
                "index",
                "--git-path",
                "objects",
            ]),
            "find the Git worktree",
        )
        .await
        .map_err(|error| {
            CliError::new(
                ErrorKind::Input,
                format!(
                    "--local snapshots the Git worktree of the current directory, but there is none: {error}"
                ),
            )
        })?;
        let mut paths = output.lines().map(PathBuf::from);
        let (Some(root), Some(index), Some(objects)) = (paths.next(), paths.next(), paths.next())
        else {
            return Err(Git::failure("find the Git worktree", "unexpected output"));
        };
        Ok(Self {
            root,
            index,
            objects,
        })
    }

    /// The branch `HEAD` is on; `None` when it is detached.
    pub(crate) async fn branch(&self) -> Result<Option<String>> {
        Git::optional(
            Git::command(&self.root).args(["symbolic-ref", "--quiet", "--short", "HEAD"]),
            "read the current branch",
        )
        .await
    }

    /// Commits the worktree's tracked and untracked files on top of `HEAD`,
    /// without changing the repository. Ignored files are left out, and
    /// tracked ones kept as committed: `git add` would stage edits to them,
    /// such as real credentials in a committed `.env` template. Submodules,
    /// nested repositories, what [`Self::check_supported`] refuses, and,
    /// when the daemon reports its `limits`, files that exceed them are
    /// refused before anything is written.
    pub(crate) async fn snapshot(
        &self,
        branch: &str,
        limits: Option<&SnapshotLimits>,
    ) -> Result<Snapshot<'_>> {
        let head = Git::run(
            Git::command(&self.root).args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
            "read HEAD",
        )
        .await
        .map_err(|error| {
            CliError::new(
                ErrorKind::Input,
                format!("HEAD has no commit yet; commit once first: {error}"),
            )
        })?;
        let files = self
            .ls_files(&["--cached", "--others"], "list the worktree's files")
            .await?;
        let ignored = self
            .ls_files(
                &["--cached", "--ignored", "--with-tree=HEAD"],
                "list tracked ignored files",
            )
            .await?;
        self.check_supported(&files).await?;
        if let Some(limits) = limits {
            self.check_size(&files, &ignored, limits).await?;
        }
        let directory = tempfile::Builder::new()
            .prefix("piquelctl-snapshot-")
            .tempdir()
            .map_err(|error| Git::failure("create a temporary directory", error))?;
        let snapshot = Snapshot {
            worktree: self,
            directory,
            commit: String::new(),
            uncommitted: Vec::new(),
        };
        tokio::fs::create_dir(snapshot.objects())
            .await
            .map_err(|error| Git::failure("create the object directory", error))?;
        match tokio::fs::copy(&self.index, snapshot.index()).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(Git::failure("copy the index", error));
            }
            _ => {}
        }
        snapshot.stage(ignored).await?;
        let tree = Git::run(&mut snapshot.staged(&["write-tree"]), "write the tree").await?;
        let uncommitted = Git::run(
            &mut snapshot.staged(&["diff-index", "--cached", "--name-only", "-z", "HEAD"]),
            "list uncommitted files",
        )
        .await?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
        let submodules = Git::run(
            &mut snapshot.staged(&["ls-files", "--stage", "-z"]),
            "list staged files",
        )
        .await?
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("160000 "))
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_owned()))
        .collect::<Vec<_>>();
        if !submodules.is_empty() {
            return Err(CliError::new(
                ErrorKind::Input,
                format!(
                    "--local does not support submodules or nested repositories: {}",
                    submodules.join(", ")
                ),
            ));
        }
        let message = format!("piquelctl snapshot of {branch}");
        let date = Git::run(
            Git::command(&self.root).args(["show", "-s", "--format=%cd", "--date=raw", &head]),
            "read HEAD's date",
        )
        .await?;
        let commit = Git::run(
            snapshot
                .staged(&["commit-tree", &tree, "-p", &head, "-m", &message])
                .env("GIT_AUTHOR_NAME", "piquelctl")
                .env("GIT_AUTHOR_EMAIL", "snapshot@piquelctl.invalid")
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_NAME", "piquelctl")
                .env("GIT_COMMITTER_EMAIL", "snapshot@piquelctl.invalid")
                .env("GIT_COMMITTER_DATE", &date),
            "commit the snapshot",
        )
        .await?;
        Ok(Snapshot {
            commit,
            uncommitted,
            ..snapshot
        })
    }

    /// The files a snapshot would send that aren't in `HEAD`, untracked or
    /// only staged, and whose names usually hold credentials (see
    /// [`Self::secret_name`]), such as `.env.local`, in path order.
    pub(crate) async fn secret_files(&self) -> Result<Vec<String>> {
        let untracked = self.ls_files(&["--others"], "list untracked files").await?;
        let staged = Git::run(
            Git::command(&self.root).args([
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--diff-filter=A",
                "HEAD",
            ]),
            "list staged files",
        )
        .await?;
        Ok(untracked
            .split('\0')
            .chain(staged.split('\0'))
            .filter(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(Self::secret_name)
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    /// Whether a file named `name` usually holds credentials, ignoring case:
    /// `.env*` but templates ending in `.example`, `.sample` or `.template`;
    /// SSH keys `id_*`; `*.pem`, `*.key`, `*.p12` and `*.pfx` keys and
    /// certificates; `credentials` and `*credentials.json`, such as gcloud's;
    /// `.npmrc`, `.pypirc`, `.netrc` and `.git-credentials`.
    fn secret_name(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        if name.starts_with(".env") {
            return ![".example", ".sample", ".template"]
                .iter()
                .any(|template| name.ends_with(template));
        }
        name.starts_with("id_")
            || [".pem", ".key", ".p12", ".pfx", "credentials.json"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            || matches!(
                name.as_str(),
                "credentials" | ".npmrc" | ".pypirc" | ".netrc" | ".git-credentials"
            )
    }

    /// The worktree's files `git ls-files` lists with `options`, such as
    /// `--cached` for tracked ones or `--others` for untracked ones,
    /// NUL-separated, honoring ignore rules.
    async fn ls_files(&self, options: &[&str], action: &str) -> Result<String> {
        Git::run(
            Git::command(&self.root)
                .args(["ls-files", "-z", "--exclude-standard"])
                .args(options),
            action,
        )
        .await
    }

    /// Refuses `files`, the snapshot's, when they take more than `limits`
    /// allow a checkout, naming the largest, so nothing is staged or packed
    /// in vain. Their size in the worktree stands for the snapshot's; the
    /// `ignored` ones, sent as committed, are left out of the estimate.
    async fn check_size(&self, files: &str, ignored: &str, limits: &SnapshotLimits) -> Result<()> {
        let ignored = ignored.split('\0').collect::<BTreeSet<_>>();
        let mut sizes = Vec::new();
        for path in files
            .split('\0')
            .filter(|path| !path.is_empty() && !ignored.contains(path))
        {
            if let Ok(metadata) = tokio::fs::symlink_metadata(self.root.join(path)).await
                && !metadata.is_dir()
            {
                sizes.push((metadata.len(), path));
            }
        }
        let directories = sizes
            .iter()
            .flat_map(|(_, path)| Path::new(path).ancestors().skip(1))
            .filter(|directory| !directory.as_os_str().is_empty())
            .collect::<BTreeSet<_>>()
            .len() as u64;
        let total = sizes
            .iter()
            .map(|(size, _)| SnapshotLimits::file_cost(*size))
            .sum::<u64>()
            .saturating_add(directories.saturating_mul(SnapshotLimits::file_cost(0)));
        let entries = sizes.len() as u64 + directories;
        if total <= limits.max_tree_bytes && entries <= limits.max_files {
            return Ok(());
        }
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        let largest = sizes
            .iter()
            .take(5)
            .map(|(size, path)| format!("{path} ({})", Bytes(*size)))
            .collect::<Vec<_>>()
            .join(", ");
        Err(CliError::new(
            ErrorKind::Input,
            format!(
                "the snapshot's {entries} files and directories take {}, more than the daemon \
                 checks out ({} entries, {}); ignore what the preview doesn't need. \
                 Largest: {largest}",
                Bytes(total),
                limits.max_files,
                Bytes(limits.max_tree_bytes),
            ),
        ))
    }

    /// Refuses what a snapshot can't take without writing to the
    /// repository or the daemon can't receive: object formats other than
    /// SHA-1; partial clones, which lack objects a bundle needs and fetch
    /// them into the repository on demand; and files staging would pass
    /// through a configured filter driver, such as Git LFS's, which store
    /// their own data in the repository.
    async fn check_supported(&self, files: &str) -> Result<()> {
        let format = Git::run(
            Git::command(&self.root).args(["rev-parse", "--show-object-format"]),
            "read the object format",
        )
        .await?;
        if format != "sha1" {
            return Err(CliError::new(
                ErrorKind::Input,
                format!("--local supports SHA-1 repositories only, not {format}"),
            ));
        }
        let partial = Git::optional(
            Git::command(&self.root).args([
                "config",
                "--name-only",
                "--get-regexp",
                r"^(extensions\.partialclone|remote\..*\.promisor)$",
            ]),
            "read whether this is a partial clone",
        )
        .await?;
        if partial.is_some() {
            return Err(CliError::new(
                ErrorKind::Input,
                "--local does not support partial clones; clone without --filter",
            ));
        }
        let Some(drivers) = Git::optional(
            Git::command(&self.root).args([
                "config",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|process)$",
            ]),
            "read the filter drivers",
        )
        .await?
        else {
            return Ok(());
        };
        let drivers = drivers
            .lines()
            .filter_map(|key| key.strip_prefix("filter.")?.rsplit_once('.'))
            .map(|(driver, _)| driver.to_owned())
            .collect::<BTreeSet<_>>();
        let attributes = Git::run_with_input(
            Git::command(&self.root).args(["check-attr", "--stdin", "-z", "filter"]),
            files.to_owned(),
            "read the filters",
        )
        .await?;
        let filtered = attributes
            .split('\0')
            .collect::<Vec<_>>()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|[_, _, driver]| drivers.contains(*driver))
            .map(|[path, _, _]| (*path).to_owned())
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            return Ok(());
        }
        Err(CliError::new(
            ErrorKind::Input,
            format!(
                "--local does not support files passed through Git filters such as LFS: {}",
                filtered.join(", ")
            ),
        ))
    }

    /// The nearest ancestor of `HEAD` that a remote-tracking branch has,
    /// which the application's repository most likely has too.
    async fn base(&self) -> Result<Option<String>> {
        let remotes = Git::run(
            Git::command(&self.root).args([
                "for-each-ref",
                "--format=%(objectname)",
                "refs/remotes",
            ]),
            "list remote-tracking branches",
        )
        .await?;
        let mut remotes = remotes.lines().collect::<Vec<_>>();
        remotes.sort_unstable();
        remotes.dedup();
        if remotes.is_empty() {
            return Ok(None);
        }
        Git::optional(
            Git::command(&self.root)
                .args(["merge-base", "HEAD"])
                .args(remotes),
            "find the base commit",
        )
        .await
    }
}

impl Bundle {
    /// What is being uploaded.
    ///
    /// ```text
    /// 12.0 KiB against 0123456789ab: 2 unpushed commits and the snapshot
    /// 3.4 MiB with the whole history: 140 commits and the snapshot
    /// ```
    pub(crate) fn describe(&self) -> String {
        let size = Bytes(self.length);
        let commits = self.commits;
        match &self.snapshot.base {
            Some(base) => format!(
                "{size} against {}: {commits} unpushed commits and the snapshot",
                short_commit(base)
            ),
            None => format!("{size} with the whole history: {commits} commits and the snapshot"),
        }
    }

    /// Refuses a bundle larger than the daemon accepts, before uploading it.
    pub(crate) fn check_size(&self, limits: Option<&SnapshotLimits>) -> Result<()> {
        match limits {
            Some(limits) if self.length > limits.max_upload_bytes => Err(CliError::new(
                ErrorKind::Input,
                format!(
                    "the snapshot is {}, more than the daemon's {} upload limit; \
                     ignore what the preview doesn't need",
                    Bytes(self.length),
                    Bytes(limits.max_upload_bytes)
                ),
            )),
            _ => Ok(()),
        }
    }
}

impl Snapshot<'_> {
    /// Bundles the snapshot with `history`.
    pub(crate) async fn bundle(&self, history: History) -> Result<Bundle> {
        let base = match history {
            History::Full => None,
            History::Thin => Some(self.worktree.base().await?.ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    "no remote-tracking branch has an ancestor of HEAD, so the daemon would \
                     need the whole history; push or fetch a base, or pass --full-history",
                )
            })?),
        };
        let repository = self.directory.path().join("bundle.git");
        if !repository.exists() {
            Git::run(
                Git::command(self.directory.path())
                    .args(["init", "--quiet", "--bare", "--object-format=sha1"])
                    .arg(&repository),
                "create the bundle repository",
            )
            .await?;
            let alternates = format!(
                "{}\n{}\n",
                self.objects().display(),
                self.worktree.objects.display()
            );
            tokio::fs::write(repository.join("objects/info/alternates"), alternates)
                .await
                .map_err(|error| Git::failure("share the snapshot's objects", error))?;
            Git::run(
                Git::command(&repository).args(["update-ref", "refs/heads/snapshot", &self.commit]),
                "name the snapshot",
            )
            .await?;
        }
        let path = self.directory.path().join(if base.is_some() {
            "thin.bundle"
        } else {
            "complete.bundle"
        });
        let mut bundle = Git::command(&repository);
        bundle
            .args(["bundle", "create", "--quiet"])
            .arg(&path)
            .arg("refs/heads/snapshot");
        if let Some(base) = &base {
            bundle.arg(format!("^{base}"));
        }
        Git::run(&mut bundle, "create the bundle").await?;
        let range = match &base {
            Some(base) => format!("{base}..HEAD"),
            None => "HEAD".to_owned(),
        };
        let commits = Git::run(
            Git::command(&self.worktree.root).args(["rev-list", "--count", &range]),
            "count the commits",
        )
        .await?
        .parse()
        .map_err(|error| Git::failure("count the commits", error))?;
        let length = tokio::fs::metadata(&path)
            .await
            .map_err(|error| Git::failure("read the bundle", error))?
            .len();
        Ok(Bundle {
            path,
            length,
            snapshot: LocalSnapshot {
                commit: self.commit.clone(),
                base,
            },
            commits,
        })
    }

    /// Stages the worktree into the snapshot's index, keeping the tracked
    /// `ignored` files, NUL-separated, as committed: they are reset to
    /// `HEAD` and marked assume-unchanged first, so staging never reads
    /// their local content.
    async fn stage(&self, ignored: String) -> Result<()> {
        let index_entries = Git::run(
            &mut self.staged(&["ls-files", "-v", "-z"]),
            "list the index",
        )
        .await?;
        let assumed = index_entries
            .split('\0')
            .filter_map(|entry| entry.split_once(' '))
            .filter(|(tag, _)| tag.bytes().all(|tag| tag.is_ascii_lowercase()))
            .map(|(_, path)| path)
            .collect::<Vec<_>>();
        if !assumed.is_empty() {
            Git::run_with_input(
                &mut self.staged(&["update-index", "--no-assume-unchanged", "-z", "--stdin"]),
                assumed.join("\0"),
                "stage files marked assume-unchanged",
            )
            .await?;
        }
        if !ignored.is_empty() {
            Git::run_with_input(
                &mut self.staged(&[
                    "--literal-pathspecs",
                    "reset",
                    "--quiet",
                    "HEAD",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ]),
                ignored,
                "keep ignored files as committed",
            )
            .await?;
            let committed = Git::run(
                &mut self.staged(&[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--ignored",
                    "--exclude-standard",
                ]),
                "list tracked ignored files",
            )
            .await?;
            if !committed.is_empty() {
                Git::run_with_input(
                    &mut self.staged(&["update-index", "--assume-unchanged", "-z", "--stdin"]),
                    committed,
                    "keep ignored files as committed",
                )
                .await?;
            }
        }
        Git::run(&mut self.staged(&["add", "--all"]), "stage the worktree").await?;
        Ok(())
    }

    /// A Git command in the worktree that reads and writes the snapshot's
    /// index and objects, reading the repository's as alternates.
    fn staged(&self, arguments: &[&str]) -> Command {
        let mut command = Git::command(&self.worktree.root);
        command
            .env("GIT_INDEX_FILE", self.index())
            .env("GIT_OBJECT_DIRECTORY", self.objects())
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.worktree.objects)
            .args(arguments);
        command
    }

    /// The snapshot's index, a copy of the worktree's.
    fn index(&self) -> PathBuf {
        self.directory.path().join("index")
    }

    /// Where Git writes the snapshot's new objects.
    fn objects(&self) -> PathBuf {
        self.directory.path().join("objects")
    }
}

/// Runs the Git commands of a snapshot.
struct Git;

impl Git {
    /// Settings that would otherwise make staging write outside the
    /// snapshot's directory: hooks such as `post-index-change`, a split
    /// index's shared files beside the repository's index, and an fsmonitor
    /// daemon.
    const ISOLATED: [&str; 6] = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.splitIndex=false",
        "-c",
        "core.fsmonitor=false",
    ];

    /// A Git command run in `directory`, unaffected by variables naming
    /// another repository, never prompting, and with [`Self::ISOLATED`].
    fn command(directory: &Path) -> Command {
        let mut command = Command::new("git");
        for variable in REPOSITORY_VARIABLES {
            command.env_remove(variable);
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(Self::ISOLATED)
            .arg("-C")
            .arg(directory)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Runs `command` to `action`, returning its output as [`Self::checked`].
    async fn run(command: &mut Command, action: &str) -> Result<String> {
        Self::checked(command.output().await, action)
    }

    /// Like [`Self::run`], but `None` when Git exits with code 1, which the
    /// commands run this way use for "nothing found".
    async fn optional(command: &mut Command, action: &str) -> Result<Option<String>> {
        match command.output().await {
            Ok(output) if output.status.code() == Some(1) => Ok(None),
            output => Self::checked(output, action).map(Some),
        }
    }

    /// Like [`Self::run`], with `input` on the command's standard input.
    async fn run_with_input(command: &mut Command, input: String, action: &str) -> Result<String> {
        let output = async {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdin = child.stdin.take().ok_or(std::io::ErrorKind::BrokenPipe)?;
            let write = async move {
                tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await
            };
            let (written, output) = tokio::join!(write, child.wait_with_output());
            written?;
            output
        };
        Self::checked(output.await, action)
    }

    /// The standard output of a successful command, without its final
    /// newline, or a failure to `action` with what Git printed. Nothing
    /// else is trimmed: NUL-separated output keeps every path intact.
    fn checked(output: std::io::Result<Output>, action: &str) -> Result<String> {
        let output = output.map_err(|error| Self::failure(action, error))?;
        if !output.status.success() {
            return Err(Self::failure(
                action,
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        String::from_utf8(output.stdout)
            .map(|stdout| stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
            .map_err(|error| Self::failure(action, error))
    }

    /// A failure to `action` while snapshotting.
    fn failure(action: &str, cause: impl std::fmt::Display) -> CliError {
        CliError::new(
            ErrorKind::General,
            format!("could not {action} for the snapshot: {cause}"),
        )
    }
}

#[cfg(test)]
mod tests;
