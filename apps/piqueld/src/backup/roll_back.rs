//! Rolling the data directory back with `piqueld roll-back`, after a newer
//! piqueld migrated its database, so this older binary can start. On NixOS
//! the unit runs it before each start, so a system rollback also rolls back
//! the daemon's state.
//!
//! The data directory itself is the unit's `StateDirectory`, so it stays:
//! its contents move into a `rolled-back-<ms>` directory inside it, which
//! `piqueld restore` and later roll-backs leave alone, then the backup is
//! restored around it.

use super::{Backups, Manifest, Moved, PreMigration, Refusal, entries, move_entries, sync};
use anyhow::{Context, Result, bail};
use piqueld_core::InstanceId;
use piqueld_store::SCHEMA_VERSION;
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

/// Directories that hold no state, only what services listening in them
/// use, which go back in place after a roll-back: the telemetry receivers'
/// sockets, which their collectors keep listening on.
const RUNTIME: [&str; 1] = ["telemetry"];

/// A directory a roll-back moved the state it replaced into,
/// `rolled-back-<ms>`, which nothing deletes.
pub(super) struct RolledBack;

impl RolledBack {
    /// The name prefix of these directories.
    const PREFIX: &str = "rolled-back-";

    /// The directory's name for a roll-back at `at_ms`.
    fn name(at_ms: i64) -> String {
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

impl Backups {
    /// Rolls the data directory back to the newest pre-migration backup this
    /// binary opens, once a newer piqueld migrated its database: verifies it
    /// as [`Self::restore`] does, calls `stop_ingress` with the instance,
    /// moves the contents into a [`RolledBack`] directory, publishes the
    /// backup, and moves [`RUNTIME`] directories back. Does nothing when this
    /// binary opens the database, or there is none.
    ///
    /// # Errors
    /// [`Refusal::NoBackup`] when no backup fits, before anything changes, or
    /// the failed step. A failed move or publish is undone.
    pub async fn roll_back(
        &self,
        stop_ingress: impl AsyncFnOnce(&InstanceId) -> Result<()>,
    ) -> Result<RollBack> {
        let data_dir = &self.data_dir;
        if !data_dir
            .try_exists()
            .with_context(|| format!("inspect {}", data_dir.display()))?
        {
            return Ok(RollBack::Unneeded);
        }
        let _lock = self.lock().await?;
        self.ensure_restore_complete()?;
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
            bail!(Refusal::NoBackup {
                schema: live.schema_version,
                directory,
            });
        };
        let backup = directory.join(newest.name());
        let manifest = Self::manifest(&backup)?;
        if manifest.instance_id != live.instance_id.as_str() {
            bail!(Refusal::OtherInstance(manifest.instance_id));
        }
        let staging = self.staging()?;
        Self::stage_restore(&backup, staging.path(), &manifest).await?;
        stop_ingress(&live.instance_id).await?;

        let aside = data_dir.join(RolledBack::name(piqueld_store::now_ms()));
        let staged = staging.path().file_name().map(OsStr::to_owned);
        let state = entries(data_dir, |name| {
            Some(name) != staged.as_deref() && !RolledBack::is(name)
        })?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&aside)
            .with_context(|| format!("create {}", aside.display()))?;
        sync(data_dir)?;
        if let Err(Moved { error, undo }) = move_entries(data_dir, &aside, &state) {
            return Err(self.unmoved(error, undo, Some(staging), &aside));
        }
        if let Err(error) = self.publish(staging) {
            if self.ensure_restore_complete().is_err() {
                return Err(
                    error.context(format!("the state it replaced is in {}", aside.display()))
                );
            }
            let undo = move_entries(&aside, data_dir, &state)
                .err()
                .map(|moved| moved.error);
            return Err(self.unmoved(error, undo, None, &aside));
        }
        let runtime: Vec<OsString> = RUNTIME
            .iter()
            .map(OsString::from)
            .filter(|name| aside.join(name).exists())
            .collect();
        move_entries(&aside, data_dir, &runtime)
            .map_err(|moved| moved.error)
            .with_context(|| format!("restored {}, then", backup.display()))?;
        Ok(RollBack::Restored {
            backup,
            manifest,
            aside,
        })
    }

    /// The error of a roll-back that failed once it started moving the
    /// state into `aside`. When the state couldn't all move back, as `undo`
    /// says, `staging`, or a new staging directory, stays as the
    /// unfinished-restore marker, so the daemon doesn't start on part of it.
    fn unmoved(
        &self,
        error: anyhow::Error,
        undo: Option<anyhow::Error>,
        staging: Option<tempfile::TempDir>,
        aside: &Path,
    ) -> anyhow::Error {
        let Some(undo) = undo else {
            return error;
        };
        let marker = staging.map_or_else(|| self.staging(), Ok);
        let error = error
            .context(format!("{undo:#}"))
            .context(format!("part of the state is still in {}", aside.display()));
        match marker {
            Ok(marker) => error.context(Refusal::Unfinished(self.data_dir.clone(), marker.keep())),
            Err(marker) => error.context(format!("{marker:#}")),
        }
    }
}
