//! Snapshots of local worktrees, uploaded by `preview deploy --local` as Git
//! bundles. Each untrusted bundle is streamed to disk within the
//! `snapshots.max_upload_bytes` setting, then received into a private repository of its own: its
//! header must name one snapshot ref, `git bundle verify` and
//! `transfer.fsckObjects` check it, its prerequisites (fetched from the
//! application's repository) must share one base the snapshot descends from, and
//! its tree must fit `[checkouts] max_bytes` (see [`Checkouts::measure`]). The
//! repository then moves to `<commit>.git`, from which deployments read their
//! manifest. Unused snapshots are removed (see [`Snapshots::prune`]).
use super::{Checkout, Checkouts, TreeTooLarge};
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use piqueld_core::manifest::{GitRepository, valid_git_commit};
use piqueld_docker::command::LoggedCommand;
use piqueld_store::StoreError;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Why an upload was refused. Details stay in the daemon's logs.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The bundle is larger than `snapshots.max_upload_bytes`.
    #[error("the snapshot bundle is larger than the {limit}-byte upload limit")]
    TooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The snapshot, or the history it needs, would not fit
    /// `[checkouts] max_bytes`.
    #[error(
        "the snapshot, or the history it needs, takes more than the {limit}-byte checkout limit"
    )]
    TreeTooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The upload is not a valid bundle of one snapshot commit.
    #[error("the upload is not a valid Git bundle of one snapshot commit: {0}")]
    Invalid(&'static str),
    /// The commits the bundle needs could not be fetched.
    #[error("the commits the bundle needs could not be fetched from the application's repository")]
    BaseUnavailable {
        /// The bundle's prerequisites.
        prerequisites: Vec<String>,
    },
    /// The body stalled, or receiving took too long.
    #[error("the snapshot upload took too long; retry it")]
    TimedOut,
    /// The upload could not be read or stored.
    #[error("the snapshot could not be received")]
    Internal(#[source] anyhow::Error),
}

impl From<TreeTooLarge> for SnapshotError {
    fn from(TreeTooLarge { limit }: TreeTooLarge) -> Self {
        Self::TreeTooLarge { limit }
    }
}

impl SnapshotError {
    /// An upload that could not be read or stored because of `error`.
    fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

/// The snapshot ref every bundle carries, and the branch its repository has.
const SNAPSHOT_BRANCH: &str = "snapshot";

/// Where uploaded snapshots are kept: `<data_dir>/snapshots`, holding one
/// bare repository per snapshot commit and `staging/` for uploads being
/// received.
pub(crate) struct Snapshots {
    /// Directory holding the snapshots.
    root: PathBuf,
    /// Held while a received snapshot moves into place and is accepted, and
    /// while unused ones are removed, so removal never races an acceptance.
    lock: tokio::sync::Mutex<()>,
    /// Permits to receive an upload (see [`Self::RECEIVING`]).
    receiving: tokio::sync::Semaphore,
}

/// A snapshot received into a staging repository, not yet in place.
pub(crate) struct Received<'a> {
    /// The snapshot commit.
    pub(crate) commit: String,
    /// The commit it was uploaded against; `None` for a complete bundle.
    pub(crate) base: Option<String>,
    /// Holds the bundle and the repository; removed when dropped.
    staging: tempfile::TempDir,
    /// Its permit to receive, held until it is kept or refused.
    _receiving: tokio::sync::SemaphorePermit<'a>,
}

impl Snapshots {
    /// Uploads received or waiting to be accepted at once, bounding the
    /// disk a burst of them takes.
    const RECEIVING: usize = 2;
    /// Longest receiving one upload may take, from its permit on, so a
    /// stalled one never holds a permit for long.
    const RECEIVE_TIMEOUT: Duration = Duration::from_secs(600);
    /// Longest the body may go without sending anything.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Opens `root`, removing uploads a previous process left half-received.
    pub(crate) async fn open(root: PathBuf) -> std::io::Result<Self> {
        Checkouts::recreate(&root.join("staging")).await?;
        Ok(Self {
            root,
            lock: tokio::sync::Mutex::new(()),
            receiving: tokio::sync::Semaphore::new(Self::RECEIVING),
        })
    }

    /// Streams `body`, which announced `length` bytes, to a staging file of at most
    /// `limit` bytes, then receives it as described in the module documentation,
    /// fetching its prerequisites from `url` and measuring its tree against
    /// `checkouts`. At most [`Self::RECEIVING`] uploads are received at once; each
    /// gets [`Self::RECEIVE_TIMEOUT`], and its body [`Self::IDLE_TIMEOUT`] between
    /// chunks.
    ///
    /// # Errors
    /// Returns [`SnapshotError`] for an oversized, malformed, or unverifiable bundle,
    /// an unavailable base, a tree too large to check out, a timeout, or a storage
    /// failure.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    pub(crate) async fn receive<E: std::error::Error + Send + Sync + 'static>(
        &self,
        body: impl Stream<Item = Result<Bytes, E>>,
        (length, limit): (Option<u64>, u64),
        url: &str,
        checkouts: &Checkouts,
    ) -> Result<Received<'_>, SnapshotError> {
        if length.is_some_and(|length| length > limit) {
            return Err(SnapshotError::TooLarge { limit });
        }
        let receiving = self
            .receiving
            .acquire()
            .await
            .map_err(SnapshotError::internal)?;
        let (commit, base, staging) = tokio::time::timeout(
            Self::RECEIVE_TIMEOUT,
            self.stage(body, limit, url, checkouts),
        )
        .await
        .map_err(|_| SnapshotError::TimedOut)??;
        Ok(Received {
            commit,
            base,
            staging,
            _receiving: receiving,
        })
    }

    /// Receives an upload into a new staging repository (see
    /// [`Self::receive`]), returning its commit, its base, and the staging
    /// directory.
    async fn stage<E: std::error::Error + Send + Sync + 'static>(
        &self,
        body: impl Stream<Item = Result<Bytes, E>>,
        limit: u64,
        url: &str,
        checkouts: &Checkouts,
    ) -> Result<(String, Option<String>, tempfile::TempDir), SnapshotError> {
        let staging = tempfile::Builder::new()
            .prefix("upload-")
            .tempdir_in(self.root.join("staging"))
            .map_err(SnapshotError::internal)?;
        let bundle = staging.path().join("snapshot.bundle");
        Self::store(body, &bundle, limit).await?;
        let header = BundleHeader::read(&bundle).await?;
        let repository = Staged::in_staging(staging.path());
        repository.init().await?;
        if !header.prerequisites.is_empty() {
            let fetch = repository.fetch(url, &header.prerequisites);
            Checkouts::bounded(staging.path(), checkouts.max_bytes(), fetch).await?;
        }
        repository.unbundle(&bundle, &header).await?;
        let base = repository.base(&header).await?;
        checkouts
            .measure(&repository.0, SNAPSHOT_BRANCH)
            .await
            .map_err(|error| match error.downcast::<TreeTooLarge>() {
                Ok(too_large) => too_large.into(),
                Err(error) => SnapshotError::Internal(error),
            })?;
        Ok((header.commit, base, staging))
    }

    /// Streams `body` to `path`, refusing more than `limit` bytes.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    async fn store<E: std::error::Error + Send + Sync + 'static>(
        body: impl Stream<Item = Result<Bytes, E>>,
        path: &Path,
        limit: u64,
    ) -> Result<(), SnapshotError> {
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(SnapshotError::internal)?;
        let mut written = 0_u64;
        let mut body = std::pin::pin!(body);
        while let Some(chunk) = tokio::time::timeout(Self::IDLE_TIMEOUT, body.next())
            .await
            .map_err(|_| SnapshotError::TimedOut)?
        {
            let chunk = chunk.map_err(SnapshotError::internal)?;
            written = written.saturating_add(chunk.len() as u64);
            if written > limit {
                return Err(SnapshotError::TooLarge { limit });
            }
            file.write_all(&chunk)
                .await
                .map_err(SnapshotError::internal)?;
        }
        file.flush().await.map_err(SnapshotError::internal)
    }

    /// Moves `received` into place, unless that commit already is, and runs
    /// `accept`, which deploys it, before any [`Self::prune`] can remove it.
    /// When `accept` fails, a snapshot this call moved into place goes again,
    /// and `accept`'s error is returned even if removing it fails.
    pub(crate) async fn keep<T, E: From<SnapshotError>>(
        &self,
        received: Received<'_>,
        accept: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let _lock = self.lock.lock().await;
        let path = self.path(&received.commit);
        let installed = !tokio::fs::try_exists(&path)
            .await
            .map_err(SnapshotError::internal)?;
        if installed {
            tokio::fs::rename(Staged::in_staging(received.staging.path()).0, &path)
                .await
                .map_err(SnapshotError::internal)?;
        }
        let accepted = accept.await;
        if accepted.is_err()
            && installed
            && let Err(error) = tokio::fs::remove_dir_all(&path).await
        {
            tracing::warn!(?error, "removing a refused snapshot failed");
        }
        accepted
    }

    /// Where a deployment of the kept snapshot `commit` of the repository
    /// at `url` checks it out from.
    pub(crate) fn source(&self, url: &str, commit: &str) -> SnapshotSource {
        SnapshotSource {
            url: url.to_owned(),
            repository: GitRepository {
                url: self.path(commit).display().to_string(),
                branch: SNAPSHOT_BRANCH.into(),
                commit: Some(commit.into()),
            },
        }
    }

    /// Removes every kept snapshot but the `needed` ones, which are read
    /// while no snapshot is being accepted.
    ///
    /// # Errors
    /// Returns `needed`'s error, or a file system error; a failed removal is
    /// retried next time.
    pub(crate) async fn prune(
        &self,
        needed: impl Future<Output = Result<Vec<String>, StoreError>>,
    ) -> Result<(), StoreError> {
        let _lock = self.lock.lock().await;
        let needed = needed.await?.into_iter().collect::<BTreeSet<_>>();
        let mut entries = tokio::fs::read_dir(&self.root)
            .await
            .map_err(StoreError::PathSource)?;
        while let Some(entry) = entries.next_entry().await.map_err(StoreError::PathSource)? {
            let name = entry.file_name();
            let Some(commit) = name.to_str().and_then(|name| name.strip_suffix(".git")) else {
                continue;
            };
            if valid_git_commit(commit) && !needed.contains(commit) {
                tokio::fs::remove_dir_all(entry.path())
                    .await
                    .map_err(StoreError::PathSource)?;
            }
        }
        Ok(())
    }

    /// Where the snapshot `commit` is kept.
    fn path(&self, commit: &str) -> PathBuf {
        self.root.join(format!("{commit}.git"))
    }
}

/// The new repository an upload is received into.
struct Staged(PathBuf);

impl Staged {
    /// Longest fetching the prerequisites may take; its process group is
    /// then killed.
    const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
    /// Most bytes of Git's output kept.
    const OUTPUT_LIMIT: usize = 4096;

    /// The repository of an upload staged in `staging`.
    fn in_staging(staging: &Path) -> Self {
        Self(staging.join("repository.git"))
    }

    /// Runs Git in the repository.
    async fn git(&self, operation: &'static str, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
        let mut command = Checkout::command_in(&self.0);
        command.args(arguments);
        Ok(LoggedCommand::output(&mut command, operation, Self::OUTPUT_LIMIT).await?)
    }

    /// Creates the bare repository.
    async fn init(&self) -> Result<(), SnapshotError> {
        let mut init = Checkout::command();
        init.args([
            "init",
            "--quiet",
            "--bare",
            "--object-format=sha1",
            "--initial-branch",
            SNAPSHOT_BRANCH,
        ])
        .arg(&self.0);
        LoggedCommand::output(&mut init, "create snapshot repository", Self::OUTPUT_LIMIT)
            .await
            .map(drop)
            .map_err(|error| SnapshotError::Internal(error.into()))
    }

    /// Fetches `prerequisites` and their history from `url`, under refs so
    /// that `git bundle verify` finds them.
    async fn fetch(&self, url: &str, prerequisites: &[String]) -> Result<(), SnapshotError> {
        let refspecs = prerequisites
            .iter()
            .map(|commit| format!("{commit}:refs/prerequisites/{commit}"))
            .collect::<Vec<_>>();
        let mut arguments = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            url,
        ];
        arguments.extend(refspecs.iter().map(String::as_str));
        let fetched = tokio::time::timeout(
            Self::FETCH_TIMEOUT,
            self.git("fetch snapshot prerequisites", &arguments),
        )
        .await;
        if matches!(fetched, Ok(Ok(_))) {
            return Ok(());
        }
        tracing::warn!(?fetched, "snapshot prerequisite fetch failed");
        Err(SnapshotError::BaseUnavailable {
            prerequisites: prerequisites.to_vec(),
        })
    }

    /// Verifies `bundle` against the repository, unbundles its snapshot ref
    /// with every object checked, and checks that `header`'s commit is what
    /// it unbundled.
    async fn unbundle(&self, bundle: &Path, header: &BundleHeader) -> Result<(), SnapshotError> {
        let bundle = bundle.to_str().ok_or(SnapshotError::Invalid("path"))?;
        let invalid = |reason| {
            move |error: anyhow::Error| {
                tracing::warn!(?error, reason, "snapshot bundle refused");
                SnapshotError::Invalid(reason)
            }
        };
        self.git(
            "verify snapshot bundle",
            &["bundle", "verify", "--quiet", bundle],
        )
        .await
        .map_err(invalid("verification failed"))?;
        let refspec = format!("{}:refs/heads/{SNAPSHOT_BRANCH}", header.reference);
        let unbundle = [
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "pack.threads=1",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            bundle,
            &refspec,
        ];
        self.git("unbundle snapshot", &unbundle)
            .await
            .map_err(invalid("its objects are invalid"))?;
        let revision = format!("{SNAPSHOT_BRANCH}^{{commit}}");
        let resolve = ["rev-parse", "--verify", "--end-of-options", &revision];
        let commit = self
            .git("resolve snapshot commit", &resolve)
            .await
            .map_err(invalid("its ref is not a commit"))?;
        if String::from_utf8_lossy(&commit).trim() != header.commit {
            return Err(SnapshotError::Invalid("its ref is not a commit"));
        }
        Ok(())
    }

    /// The base of an unbundled snapshot: the one prerequisite every other
    /// is an ancestor of, which the snapshot commit descends from. `None`
    /// for a complete bundle.
    async fn base(&self, header: &BundleHeader) -> Result<Option<String>, SnapshotError> {
        let invalid = |error| {
            tracing::warn!(?error, "snapshot prerequisites refused");
            SnapshotError::Invalid("its prerequisites are not the history of one base")
        };
        if header.prerequisites.is_empty() {
            return Ok(None);
        }
        let mut independent = vec!["merge-base", "--independent"];
        independent.extend(header.prerequisites.iter().map(String::as_str));
        let bases = self
            .git("find snapshot base", &independent)
            .await
            .map_err(invalid)?;
        let bases = String::from_utf8_lossy(&bases);
        let [base] = bases.split_whitespace().collect::<Vec<_>>()[..] else {
            return Err(invalid(anyhow::anyhow!(
                "several independent prerequisites"
            )));
        };
        self.git(
            "check snapshot base",
            &["merge-base", "--is-ancestor", base, &header.commit],
        )
        .await
        .map_err(invalid)?;
        Ok(Some(base.to_owned()))
    }
}

/// A kept snapshot that a deployment checks its manifest and `"self"`
/// sources out from, instead of its repository.
pub(crate) struct SnapshotSource {
    /// The application's repository the snapshot was uploaded for.
    url: String,
    /// The snapshot's own repository, at the snapshot commit.
    pub(crate) repository: GitRepository,
}

impl SnapshotSource {
    /// Where to check `repository` out from: the snapshot for its own
    /// commit of the application's repository, which is how deployments pin
    /// `"self"` sources, and `repository` itself otherwise.
    pub(crate) fn checkout<'a>(
        source: Option<&'a Self>,
        repository: &'a GitRepository,
    ) -> &'a GitRepository {
        match source {
            Some(source)
                if repository.url == source.url
                    && repository.commit == source.repository.commit =>
            {
                &source.repository
            }
            _ => repository,
        }
    }
}

/// What a bundle's header says it carries: one snapshot ref and the commits
/// it needs. Its pack is checked by Git.
#[derive(Debug, Eq, PartialEq)]
struct BundleHeader {
    /// The ref, under `refs/`.
    reference: String,
    /// The commit the ref points at.
    commit: String,
    /// The commits it needs, at most [`Self::PREREQUISITES`].
    prerequisites: Vec<String>,
}

impl BundleHeader {
    /// Most bytes a bundle header may take.
    const HEADER_LIMIT: u64 = 64 * 1024;
    /// Most prerequisites a bundle may need. A thin bundle needs its base,
    /// and one more commit for each merge of a branch forked before it.
    const PREREQUISITES: usize = 64;

    /// Reads and checks the header of the bundle at `path`:
    ///
    /// ```text
    /// # v2 git bundle                       (or v3, with only @object-format=sha1)
    /// -<commit> <subject>                   prerequisites, at most 64
    /// <commit> refs/heads/snapshot          exactly one ref
    /// (blank line, then the pack)
    /// ```
    async fn read(path: &Path) -> Result<Self, SnapshotError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(SnapshotError::internal)?;
        let mut lines =
            tokio::io::AsyncReadExt::take(tokio::io::BufReader::new(file), Self::HEADER_LIMIT)
                .lines();
        let mut header = Vec::new();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if line.is_empty() => break,
                Ok(Some(line)) => header.push(line),
                Ok(None) | Err(_) => return Err(SnapshotError::Invalid("its header is malformed")),
            }
        }
        Self::parse(&header).ok_or(SnapshotError::Invalid("its header is malformed"))
    }

    /// Parses header lines, `None` unless they describe one snapshot.
    fn parse(lines: &[String]) -> Option<Self> {
        let (signature, lines) = lines.split_first()?;
        let v3 = match signature.as_str() {
            "# v2 git bundle" => false,
            "# v3 git bundle" => true,
            _ => return None,
        };
        let (mut prerequisites, mut reference) = (BTreeSet::new(), None);
        for line in lines {
            if let Some(capability) = line.strip_prefix('@') {
                if !v3 || capability != "object-format=sha1" {
                    return None;
                }
            } else if let Some(prerequisite) = line.strip_prefix('-') {
                let commit = prerequisite.split(' ').next()?;
                if !valid_git_commit(commit)
                    || !prerequisites.insert(commit.to_owned())
                    || prerequisites.len() > Self::PREREQUISITES
                {
                    return None;
                }
            } else {
                let (commit, name) = line.split_once(' ')?;
                if !valid_git_commit(commit)
                    || !Self::valid_reference(name)
                    || reference.replace((name, commit)).is_some()
                {
                    return None;
                }
            }
        }
        let (reference, commit) = reference?;
        Some(Self {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            prerequisites: prerequisites.into_iter().collect(),
        })
    }

    /// Whether `name` is a plain ref under `refs/`, safe in a refspec.
    fn valid_reference(name: &str) -> bool {
        name.strip_prefix("refs/").is_some_and(|rest| {
            !rest.is_empty()
                && rest.split('/').all(|part| {
                    !part.is_empty()
                        && !part.starts_with('.')
                        && part
                            .rsplit_once('.')
                            .is_none_or(|(_, suffix)| suffix != "lock")
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
                && !rest.contains("..")
        })
    }
}

#[cfg(test)]
mod tests;
