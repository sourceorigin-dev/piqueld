//! Rolling the data directory back with `piqueld roll-back`, after a newer
//! piqueld migrated its database, so this older binary can start. On NixOS
//! the unit runs it before each start, so a system rollback also rolls back
//! the daemon's state.
//!
//! The data directory itself is the unit's `StateDirectory`, so it stays:
//! its contents but [`Backups::kept`] ones move into a `rolled-back-<ms>`
//! directory inside it, then the backup is restored around them.

use super::{BackupError, Result};
use super::{Backups, Manifest, PreMigration, Refusal, entries, millis_after};
use crate::ingress::IngressError;
use piqueld_core::InstanceId;
use piqueld_store::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

/// A directory a roll-back moved the state it replaced into,
/// `rolled-back-<ms>`. Roll-backs keep the newest [`Self::KEEP`].
pub(super) struct RolledBack;

impl RolledBack {
    /// The name prefix of these directories.
    const PREFIX: &str = "rolled-back-";
    /// How many generated directories the data directory keeps, the newest
    /// roll-back's included.
    const KEEP: usize = 3;

    /// Deletes all but the newest `keep` generated directories in
    /// `data_dir`. Others, such as one a person named, stay.
    fn prune(data_dir: &Path, keep: usize) -> Result<()> {
        let mut generated: Vec<_> =
            entries(data_dir, |name| millis_after(name, Self::PREFIX).is_some())?
                .into_iter()
                .filter_map(|name| Some((millis_after(&name, Self::PREFIX)?, name)))
                .collect();
        generated.sort_unstable_by(|left, right| right.cmp(left));
        for (_, name) in generated.into_iter().skip(keep) {
            let path = data_dir.join(name);
            fs::remove_dir_all(&path)
                .map_err(BackupError::io("delete the old rolled-back state", &path))?;
        }
        Ok(())
    }

    /// The directory's name for a roll-back at `at_ms`.
    pub(super) fn name(at_ms: i64) -> String {
        format!("{}{at_ms}", Self::PREFIX)
    }

    /// Whether the data directory's entry `name` is one of these.
    #[must_use]
    pub(super) fn is(name: &OsStr) -> bool {
        name.as_encoded_bytes().starts_with(Self::PREFIX.as_bytes())
    }
}

/// What [`Backups::roll_back`] did.
#[derive(Debug)]
pub enum RollBack {
    /// This binary opens the database, or there is none: nothing changed.
    Unneeded,
    /// An interrupted restore or roll-back was finished, rather than a new
    /// one started.
    Finished,
    /// The state was rolled back.
    Restored {
        /// The backup restored.
        backup: PathBuf,
        /// Its manifest.
        manifest: Manifest,
        /// The directory holding the state it replaced.
        aside: PathBuf,
    },
}

/// What a roll-back did, left in the data directory for the daemon to report
/// once it starts on the restored state.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RollBackReport {
    /// The name of the backup restored.
    pub backup: String,
    /// The schema version of the database it replaced.
    pub from_schema: u64,
    /// The schema version it restored.
    pub to_schema: u64,
    /// The directory holding the state it replaced.
    pub replaced: PathBuf,
}

impl RollBackReport {
    /// Its file in the data directory, which restores and roll-backs leave
    /// in place.
    pub(super) const FILE: &str = "roll-back.json";

    /// What it says, for people.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "piqueld rolled back to backup {}, from schema {} to {}; the state it replaced is in {}",
            self.backup,
            self.from_schema,
            self.to_schema,
            self.replaced.display()
        )
    }

    /// Writes it into `data_dir`, privately and atomically.
    pub(super) async fn write(&self, data_dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(BackupError::EncodeReport)?;
        Ok(
            piqueld_config::directories::PrivateFile::at(data_dir.join(Self::FILE))
                .write(bytes)
                .await?,
        )
    }

    /// Removes the report in `data_dir`, durably, if there is one.
    async fn remove(data_dir: &Path) -> Result<()> {
        Ok(
            piqueld_config::directories::PrivateFile::at(data_dir.join(Self::FILE))
                .remove()
                .await?,
        )
    }

    /// `error`, once the report in `data_dir` is removed, or with why it
    /// couldn't be.
    async fn remove_after(data_dir: &Path, error: BackupError) -> BackupError {
        match Self::remove(data_dir).await {
            Ok(()) => error,
            Err(BackupError::Report(removal)) => BackupError::ReportNotRemoved {
                removal,
                source: Box::new(error),
            },
            Err(removal) => removal,
        }
    }

    /// The report in `data_dir`, if a roll-back left one.
    pub(super) async fn read(data_dir: &Path) -> Result<Option<Self>> {
        let path = data_dir.join(Self::FILE);
        let bytes = match tokio::fs::read(&path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            read => read.map_err(BackupError::io("read", &path))?,
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| BackupError::Parse { path, source })
    }
}

impl Backups {
    /// Reports the roll-back that left a [`RollBackReport`] in the data
    /// directory, if any: logs a warning, and records a `daemon_rolled_back`
    /// event, which notifies, then removes the report. A report whose
    /// rolled-back directory is missing, left by a roll-back stopped before
    /// it began, is removed unsent. Call it once the daemon has started on
    /// the restored state.
    ///
    /// # Errors
    /// Returns the failed step, and the report stays, to be sent on the next
    /// start, unless it can't be parsed, which removes it. The daemon logs it
    /// rather than stopping: an unsent report must not keep it down.
    pub async fn report_roll_back(&self, store: &piqueld_store::Store) -> Result<()> {
        let report = match RollBackReport::read(&self.data_dir).await {
            Ok(Some(report)) => report,
            Ok(None) => return Ok(()),
            Err(error @ BackupError::Parse { .. }) => {
                let error = BackupError::UnparsableReport(Box::new(error));
                return Err(RollBackReport::remove_after(&self.data_dir, error).await);
            }
            Err(error) => return Err(error),
        };
        let replaced = &report.replaced;
        if !tokio::fs::try_exists(replaced)
            .await
            .map_err(BackupError::io("inspect", replaced))?
        {
            return RollBackReport::remove(&self.data_dir).await;
        }
        let message = report.message();
        tracing::warn!(backup = %report.backup, replaced = %report.replaced.display(), "{message}");
        store
            .record_daemon_rolled_back(&report.backup, &message)
            .await
            .map_err(BackupError::RecordRollBack)?;
        RollBackReport::remove(&self.data_dir).await
    }

    /// Finishes an unfinished restore, if any. Otherwise, once a newer piqueld
    /// migrated the database, rolls it back to the newest pre-migration
    /// backup this binary opens: verifies it as [`Self::restore`] does, calls
    /// `stop_ingress` with the instance, makes room among the [`RolledBack`]
    /// directories, and [finishes](Self::finish) it. Does nothing when this
    /// binary opens the database, or there is none.
    ///
    /// # Errors
    /// [`Refusal::NoBackup`] when no backup fits, before anything changes, or
    /// the failed step. Once state has started moving, a failure leaves the
    /// restore unfinished, for the next run to finish.
    pub async fn roll_back(
        &self,
        stop_ingress: impl AsyncFnOnce(&InstanceId) -> std::result::Result<(), IngressError>,
    ) -> Result<RollBack> {
        let data_dir = &self.data_dir;
        if !data_dir
            .try_exists()
            .map_err(BackupError::io("inspect", data_dir))?
        {
            return Ok(RollBack::Unneeded);
        }
        let _lock = self.lock().await?;
        if let Some(staging) = self.unfinished()? {
            self.finish(staging)?;
            return Ok(RollBack::Finished);
        }
        let Some(live) = self
            .identity()
            .await?
            .filter(|identity| identity.schema_version > SCHEMA_VERSION)
        else {
            return Ok(RollBack::Unneeded);
        };

        let directory = self.directory();
        let Some(newest) = PreMigration::list(&directory)?
            .into_iter()
            .find(|backup| backup.schema_version <= SCHEMA_VERSION)
        else {
            return Err(Refusal::NoBackup {
                schema: live.schema_version,
                directory,
            }
            .into());
        };
        let backup = directory.join(newest.name());
        let manifest = Self::manifest(&backup)?;
        if manifest.instance_id != live.instance_id.as_str() {
            return Err(Refusal::OtherInstance(manifest.instance_id).into());
        }
        let staged = self.copy_verified(&backup, &manifest).await?;
        stop_ingress(&live.instance_id).await?;
        RolledBack::prune(data_dir, RolledBack::KEEP - 1)?;

        let at_ms = piqueld_store::now_ms();
        let aside = data_dir.join(RolledBack::name(at_ms));
        RollBackReport {
            backup: newest.name(),
            from_schema: live.schema_version,
            to_schema: manifest.schema_version,
            replaced: aside.clone(),
        }
        .write(data_dir)
        .await?;
        let staging = match self.mark(staged, at_ms) {
            Ok(staging) => staging,
            Err(error) => return Err(RollBackReport::remove_after(data_dir, error).await),
        };
        self.finish(staging)
            .map_err(|source| BackupError::RollBack {
                backup: backup.clone(),
                aside: aside.clone(),
                source: Box::new(source),
            })?;
        Ok(RollBack::Restored {
            backup,
            manifest,
            aside,
        })
    }
}
