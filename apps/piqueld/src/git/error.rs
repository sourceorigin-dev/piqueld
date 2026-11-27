//! Why a Git listing, checkout or build fails.

use super::TreeTooLarge;
use piqueld_docker::{DockerError, command::CommandError};
use std::{
    io,
    num::{ParseIntError, TryFromIntError},
    path::PathBuf,
    process::ExitStatus,
    string::FromUtf8Error,
};
use thiserror::Error;
use tokio::{sync::AcquireError, task::JoinError, time::error::Elapsed};

/// A step of restoring a checkout's modes that failed.
#[derive(Debug, Error)]
#[error("{action} {}", path.display())]
pub struct ModeError {
    /// What was attempted, such as `set the mode of`.
    pub(super) action: &'static str,
    /// The file or directory.
    pub(super) path: PathBuf,
    /// The file system failure.
    #[source]
    pub(super) source: io::Error,
}

/// Why a Git listing, checkout or build failed.
#[derive(Debug, Error)]
pub enum GitError {
    /// `git ls-remote` outlived [`Heads::TIMEOUT`](super::Heads).
    #[error("git ls-remote timed out")]
    ListTimedOut(#[source] Elapsed),
    /// `git ls-remote` exited unsuccessfully. Git's output, which can name
    /// the repository's credentials, is logged and never carried here.
    #[error("git ls-remote failed ({status})")]
    ListFailed {
        /// How it exited.
        status: ExitStatus,
    },
    /// `git ls-remote` did not run to completion. The cause is logged and
    /// never carried here.
    #[error("git ls-remote could not run")]
    ListNotRun,
    /// The listing is not UTF-8.
    #[error("decode git ls-remote output")]
    DecodeListing(#[source] FromUtf8Error),
    /// A listing line has no tab between its commit and reference.
    #[error("parse git ls-remote output")]
    ParseListing,
    /// The application has no manifest repository to list.
    #[error("the application has no manifest repository")]
    NoManifestRepository,
    /// The repository configuration fails validation.
    #[error("invalid Git repository configuration")]
    InvalidRepository,
    /// The checkout or listing semaphore was closed.
    #[error("{0}")]
    Admission(#[from] AcquireError),
    /// The checkout's temporary directory could not be created.
    #[error("create Git checkout directory")]
    CreateDirectory(#[source] io::Error),
    /// A Git command failed.
    #[error("{0}")]
    Command(#[from] CommandError),
    /// The checkout is larger than admitted.
    #[error("{0}")]
    TooLarge(#[from] TreeTooLarge),
    /// The checkout's size needs more disk permits than exist.
    #[error("count the checkout's disk permits")]
    DiskPermits(#[source] TryFromIntError),
    /// `.git/info` could not be created.
    #[error("create Git info directory")]
    CreateInfo(#[source] io::Error),
    /// The checkout attributes could not be written.
    #[error("write Git checkout attributes")]
    WriteAttributes(#[source] io::Error),
    /// The tree sizes are not UTF-8.
    #[error("decode Git tree sizes")]
    DecodeSizes(#[source] FromUtf8Error),
    /// A blob size is not a number.
    #[error("parse a Git blob size")]
    ParseSize(#[source] ParseIntError),
    /// Walking the checkout to measure it failed.
    #[error("{0}")]
    DiskUsage(#[source] io::Error),
    /// The measuring task failed.
    #[error("measure Git checkout")]
    MeasureTask(#[source] JoinError),
    /// `git rev-parse HEAD` could not run.
    #[error("resolve Git commit")]
    ResolveCommit(#[source] io::Error),
    /// `git rev-parse HEAD` printed something that is not UTF-8.
    #[error("decode Git commit")]
    DecodeCommit(#[source] FromUtf8Error),
    /// `HEAD` is not a full commit hash.
    #[error("Git did not resolve a full commit hash")]
    NoCommit,
    /// The checkout's modes could not be restored.
    #[error("restore Git checkout modes")]
    RestoreModes(#[source] ModeError),
    /// The mode-restoring task failed.
    #[error("join the Git checkout mode restoration")]
    RestoreTask(#[source] JoinError),
    /// The Docker build of the checkout failed.
    #[error("build Git source image")]
    Build(#[source] DockerError),
    /// A manifest path leaves the repository.
    #[error("path must remain within the repository root")]
    PathOutside,
    /// The checkout's root could not be resolved.
    #[error("locate checkout root")]
    LocateRoot(#[source] io::Error),
    /// A repository file could not be resolved.
    #[error("locate repository file")]
    LocateFile(#[source] io::Error),
    /// A path resolves outside the checkout, or into `.git`.
    #[error("repository path escapes the checkout")]
    PathEscapes,
}

impl GitError {
    /// Whether a file or the checkout's root was missing, which a manifest
    /// reads as not found.
    #[must_use]
    pub fn not_found(&self) -> bool {
        matches!(
            self,
            Self::LocateRoot(error) | Self::LocateFile(error)
                if error.kind() == io::ErrorKind::NotFound
        )
    }
}
