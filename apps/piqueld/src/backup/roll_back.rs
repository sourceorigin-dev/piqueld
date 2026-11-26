//! Rolling the data directory back with `piqueld roll-back`, after a newer
//! piqueld migrated its database, so this older binary can start. On NixOS
//! the unit runs it before each start, so a system rollback also rolls back
//! the daemon's state.
//!
//! The data directory itself is the unit's `StateDirectory`, so it stays:
//! its contents but [`Backups::kept`] ones move into a `rolled-back-<ms>`
//! directory inside it, then the backup is restored around them.

use super::{Backups, Manifest, Moved, PreMigration, Refusal, entries, move_entries};
use anyhow::{Context, Result, bail};
use piqueld_core::InstanceId;
use piqueld_store::SCHEMA_VERSION;
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::DirBuilderExt,
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

    /// When the directory `name` was made, in Unix milliseconds, if a
    /// roll-back generated its name.
    fn made_at(name: &OsStr) -> Option<i64> {
        let digits = name.to_str()?.strip_prefix(Self::PREFIX)?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    /// Deletes all but the newest `keep` generated directories in
    /// `data_dir`. Others, such as one a person named, stay.
    fn prune(data_dir: &Path, keep: usize) -> Result<()> {
        let mut generated: Vec<_> = entries(data_dir, |name| Self::made_at(name).is_some())?
            .into_iter()
            .filter_map(|name| Some((Self::made_at(&name)?, name)))
            .collect();
        generated.sort_unstable_by(|left, right| right.cmp(left));
        for (_, name) in generated.into_iter().skip(keep) {
            let path = data_dir.join(name);
            fs::remove_dir_all(&path)
                .with_context(|| format!("delete the old rolled-back state {}", path.display()))?;
        }
        Ok(())
    }

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
    /// binary opens, once a newer piqueld migrated its database: verifies the
    /// backup as [`Self::restore`] does, calls `stop_ingress` with the
    /// instance, makes room among the [`RolledBack`] directories, moves the
    /// state into a new one, and publishes the backup. Does nothing when this binary opens the database, or there is
    /// none.
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
        let staged = self.copy_verified(&backup, &manifest).await?;
        stop_ingress(&live.instance_id).await?;
        RolledBack::prune(data_dir, RolledBack::KEEP - 1)?;

        let aside = data_dir.join(RolledBack::name(piqueld_store::now_ms()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&aside)
            .with_context(|| format!("create {}", aside.display()))?;
        let staging = match self.mark(staged) {
            Ok(staging) => staging,
            Err(error) => return Err(Self::without_aside(error, &aside)),
        };
        let state = entries(data_dir, |name| {
            Some(name) != staging.file_name() && !Self::kept(name)
        })?;
        let published = move_entries(data_dir, &aside, &state).and_then(|()| {
            self.publish(&staging).map_err(|failed| match failed.undo {
                Some(_) => failed,
                None => Moved {
                    undo: move_entries(&aside, data_dir, &state)
                        .err()
                        .map(Moved::into_error),
                    ..failed
                },
            })
        });
        let undone = published
            .as_ref()
            .is_err_and(|failed| failed.undo.is_none());
        if let Err(error) = self.finish(&staging, published) {
            return Err(if undone {
                Self::without_aside(error, &aside)
                    .context(format!("roll back to {}", backup.display()))
            } else {
                error.context(format!(
                    "roll back to {}, with the state it replaced in {}",
                    backup.display(),
                    aside.display()
                ))
            });
        }
        Ok(RollBack::Restored {
            backup,
            manifest,
            aside,
        })
    }

    /// `error`, once the empty `aside` a failed roll-back left is removed, or
    /// with why it couldn't be.
    fn without_aside(error: anyhow::Error, aside: &Path) -> anyhow::Error {
        match fs::remove_dir(aside) {
            Ok(()) => error,
            Err(removal) => error.context(format!("remove {}: {removal}", aside.display())),
        }
    }
}
