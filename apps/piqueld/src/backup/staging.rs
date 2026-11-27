//! An unfinished restore's staging directory, and finishing it. A restore,
//! or a roll-back, first verifies a copy of its backup, which then becomes a
//! [`Staging`] directory in the data directory. While one exists, the daemon
//! refuses to start, and [`Backups::finish`] finishes the restore it marks,
//! whether it just started or a crash interrupted it.

use super::{BackupError, Result, StagingKept};
use super::{Backups, Refusal, RolledBack, entries, millis_after, move_entries, sync};
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

/// An unfinished restore's staging directory, holding only a copy of its
/// backup. Named for how far the restore got, and for when it started, which
/// a roll-back's [`RolledBack`] directory shares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Staging {
    /// When the restore started, in Unix milliseconds.
    pub(super) at_ms: i64,
    /// Whether the backup is moving into place, the state having moved
    /// aside.
    pub(super) publishing: bool,
}

impl Staging {
    /// The name prefix of one whose state is moving aside.
    const MOVING: &str = ".restore-";
    /// The name prefix of one whose backup is moving into place.
    const PUBLISHING: &str = ".publish-";

    /// Its name in the data directory.
    fn name(self) -> String {
        let prefix = if self.publishing {
            Self::PUBLISHING
        } else {
            Self::MOVING
        };
        format!("{prefix}{}", self.at_ms)
    }

    /// The staging directory named `name`, if it is one.
    pub(super) fn parse(name: &OsStr) -> Option<Self> {
        let (at_ms, publishing) = match millis_after(name, Self::PUBLISHING) {
            Some(at_ms) => (at_ms, true),
            None => (millis_after(name, Self::MOVING)?, false),
        };
        Some(Self { at_ms, publishing })
    }

    /// Its path in `data_dir`.
    pub(super) fn path(self, data_dir: &Path) -> PathBuf {
        data_dir.join(self.name())
    }
}

impl Backups {
    /// The data directory's unfinished restore, if any.
    pub(super) fn unfinished(&self) -> Result<Option<Staging>> {
        Ok(
            entries(&self.data_dir, |name| Staging::parse(name).is_some())?
                .first()
                .and_then(|name| Staging::parse(name)),
        )
    }

    /// Refuses to go on while the data directory holds an unfinished
    /// restore, so the daemon never starts on partly restored state.
    ///
    /// # Errors
    /// Returns [`Refusal::Unfinished`], or the error reading the directory.
    pub fn ensure_restore_complete(&self) -> Result<()> {
        match self.unfinished()? {
            Some(staging) => {
                Err(Refusal::Unfinished(self.data_dir.clone(), staging.path(&self.data_dir)).into())
            }
            None => Ok(()),
        }
    }

    /// Makes the verified copy `staged` the staging directory of a restore
    /// started at `at_ms`, which marks it unfinished until [`Self::finish`]
    /// removes it. If that can't be synced, removes it again.
    pub(super) fn mark(&self, staged: tempfile::TempDir, at_ms: i64) -> Result<Staging> {
        let data_dir = &self.data_dir;
        let staging = Staging {
            at_ms,
            publishing: false,
        };
        let path = staging.path(data_dir);
        fs::rename(staged.path(), &path)
            .map_err(BackupError::io("move the verified backup to", &path))?;
        drop(staged.keep());
        let Err(error) = sync(data_dir) else {
            return Ok(staging);
        };
        Err(match fs::remove_dir_all(&path) {
            Ok(()) => error,
            Err(removal) => BackupError::Unfinished {
                data_dir: data_dir.clone(),
                staging: path.clone(),
                source: Box::new(StagingKept {
                    path,
                    removal,
                    source: Box::new(error),
                }),
            },
        })
    }

    /// Finishes the restore `staging` marks, from where it got: moves the
    /// data directory's state, but [`Self::kept`] entries, into the
    /// [`RolledBack`] directory it shares a time with, then moves the backup
    /// into place and removes `staging`. Each step is synced, and a failed
    /// one leaves `staging`, so running it again carries on.
    ///
    /// # Errors
    /// The failed step.
    pub(super) fn finish(&self, staging: Staging) -> Result<()> {
        let data_dir = &self.data_dir;
        let aside = data_dir.join(RolledBack::name(staging.at_ms));
        let staging = if staging.publishing {
            staging
        } else {
            let state = entries(data_dir, |name| {
                Staging::parse(name).is_none() && !Self::kept(name)
            })?;
            if !state.is_empty() {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&aside)
                    .or_else(|error| match error.kind() {
                        std::io::ErrorKind::AlreadyExists => Ok(()),
                        _ => Err(error),
                    })
                    .map_err(BackupError::io("create", &aside))?;
                sync(data_dir)?;
                move_entries(data_dir, &aside, &state)?;
            }
            let publishing = Staging {
                publishing: true,
                ..staging
            };
            fs::rename(staging.path(data_dir), publishing.path(data_dir)).map_err(|source| {
                BackupError::Publish {
                    path: staging.path(data_dir),
                    source,
                }
            })?;
            sync(data_dir)?;
            publishing
        };
        let path = staging.path(data_dir);
        let names = entries(&path, |_| true)?;
        move_entries(&path, data_dir, &names)?;
        fs::remove_dir(&path).map_err(BackupError::io("remove", &path))?;
        sync(data_dir)
    }
}
