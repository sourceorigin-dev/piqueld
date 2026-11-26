//! Why a backup, restore or roll-back fails.

use super::{Damaged, Refusal};
use crate::ingress::IngressError;
use piqueld_config::directories::PrivateFileError;
use piqueld_core::{ErrorChain as _, resource::InstanceIdError};
use piqueld_store::StoreError;
use std::{
    io,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::task::JoinError;

/// Why a backup, restore or roll-back failed.
#[derive(Debug, Error)]
pub enum BackupError {
    /// The backup or data directory is one the operation refuses.
    #[error(transparent)]
    Refusal(#[from] Refusal),
    /// The database failed its integrity check.
    #[error(transparent)]
    Damaged(#[from] Damaged),
    /// A file system step failed.
    #[error("{action} {}", path.display())]
    Io {
        /// What was attempted, such as `read` or `move the backup to`.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// A temporary directory could not be created.
    #[error("create a temporary directory")]
    TempDir(#[source] io::Error),
    /// A directory entry could not be moved.
    #[error("move {} into {}", from.display(), to.display())]
    Move {
        /// The entry.
        from: PathBuf,
        /// The directory it moves into.
        to: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// An unfinished restore's staging directory could not be renamed.
    #[error("move {} to publish it", path.display())]
    Publish {
        /// The staging directory.
        path: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// A file could not be copied.
    #[error("copy {} to {}", from.display(), to.display())]
    Copy {
        /// The source.
        from: PathBuf,
        /// The target.
        to: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// Something that must be a regular file is not.
    #[error("{} is not a regular file", .0.display())]
    NotRegularFile(PathBuf),
    /// A database file could not be used.
    #[error("{action} {}", path.display())]
    Database {
        /// What was attempted, such as `open` or `check`.
        action: &'static str,
        /// The database file.
        path: PathBuf,
        /// The failure.
        #[source]
        source: StoreError,
    },
    /// A step on the live database failed.
    #[error("{action}")]
    Live {
        /// What was attempted, such as `snapshot the database`.
        action: &'static str,
        /// The failure.
        #[source]
        source: StoreError,
    },
    /// The copied database has no schema.
    #[error("the database has no schema yet")]
    NoSchema,
    /// The secret key does not decrypt the database.
    #[error("{} does not match the database", super::SECRET_KEY)]
    KeyMismatch(#[source] StoreError),
    /// Copying the data directory or a backup failed.
    #[error("{action}")]
    CopyTask {
        /// `copy the data directory` or `copy the backup`.
        action: &'static str,
        /// The task failure.
        #[source]
        source: JoinError,
    },
    /// The manifest could not be written.
    #[error("write {}", path.display())]
    WriteManifest {
        /// The manifest file.
        path: PathBuf,
        /// The encoding failure.
        #[source]
        source: serde_json::Error,
    },
    /// A JSON file could not be parsed.
    #[error("parse {}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The decoding failure.
        #[source]
        source: serde_json::Error,
    },
    /// The manifest's instance ID is invalid.
    #[error("parse the manifest's instance ID")]
    ParseInstance(#[source] InstanceIdError),
    /// The roll-back report could not be encoded.
    #[error("encode the roll-back report")]
    EncodeReport(#[source] serde_json::Error),
    /// The roll-back report could not be written or removed.
    #[error(transparent)]
    Report(#[from] PrivateFileError),
    /// The roll-back report could not be removed after another failure.
    #[error("{}", removal.chain())]
    ReportNotRemoved {
        /// Why it could not be removed.
        removal: PrivateFileError,
        /// The failure that led to removing it.
        #[source]
        source: Box<BackupError>,
    },
    /// The roll-back report did not parse, so it was removed.
    #[error("removed the roll-back report it can't parse")]
    UnparsableReport(#[source] Box<BackupError>),
    /// The roll-back could not be recorded.
    #[error("record the roll-back")]
    RecordRollBack(#[source] StoreError),
    /// Stopping the ingress containers failed.
    #[error(transparent)]
    StopIngress(#[from] IngressError),
    /// Finishing a roll-back stopped part way.
    #[error("roll back to {}, with the state it replaced in {}", backup.display(), aside.display())]
    RollBack {
        /// The backup restored.
        backup: PathBuf,
        /// The directory holding the state it replaced.
        aside: PathBuf,
        /// The failed step.
        #[source]
        source: Box<BackupError>,
    },
    /// A restore stopped part way, and its staging directory could not be
    /// removed either.
    #[error("{}", Refusal::Unfinished(data_dir.clone(), staging.clone()))]
    Unfinished {
        /// The data directory.
        data_dir: PathBuf,
        /// The staging directory left behind.
        staging: PathBuf,
        /// Why the staging directory stayed.
        #[source]
        source: Box<StagingKept>,
    },
}

/// Why a restore's staging directory could not be removed after a failure.
#[derive(Debug, Error)]
#[error("remove {}: {removal}", path.display())]
pub struct StagingKept {
    /// The staging directory.
    pub(super) path: PathBuf,
    /// Why it could not be removed.
    pub(super) removal: io::Error,
    /// The failure that led to removing it.
    #[source]
    pub(super) source: Box<BackupError>,
}

impl BackupError {
    /// A file system failure while `action` on `path`; the errno of rustix
    /// converts too.
    pub(super) fn io<E: Into<io::Error>>(
        action: &'static str,
        path: &Path,
    ) -> impl FnOnce(E) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source: source.into(),
        }
    }

    /// A failure of the database at `path` while `action`.
    pub(super) fn database(action: &'static str, path: &Path) -> impl FnOnce(StoreError) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Database {
            action,
            path,
            source,
        }
    }

    /// A failure of the live database while `action`.
    pub(super) fn live(action: &'static str) -> impl FnOnce(StoreError) -> Self {
        move |source| Self::Live { action, source }
    }
}
