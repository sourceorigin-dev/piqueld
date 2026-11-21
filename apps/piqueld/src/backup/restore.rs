//! Restoring a backup into an empty data directory with `piqueld restore`,
//! while the daemon is stopped.
//!
//! The backup is copied into a `.restore-*` staging directory inside the
//! data directory and verified there: its database's integrity, its schema
//! and instance against the manifest, and its secret key against the
//! database. Only then is each entry moved into place, and the staging
//! directory removed last. While one exists, the restore is unfinished and
//! the daemon refuses to start (see [`Backups::ensure_restore_complete`]).

use super::{
    Backups, DATABASE, FORMAT, MANIFEST, Manifest, SECRET_KEY, TREES, copy_file, copy_tree, sync,
    verify,
};
use anyhow::{Context, Result, anyhow, bail};
use piqueld_store::{DatabaseIdentity, SCHEMA_VERSION};
use rustix::fs::CWD;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The name prefix of a restore's staging directory.
pub(super) const STAGING: &str = ".restore-";

/// Why a backup can't be restored.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// The backup's layout is newer than this binary knows.
    #[error("backup format {0} is not supported; use a newer piqueld")]
    Format(u32),
    /// The backup's database is newer than this binary supports.
    #[error(
        "the backup's schema version {0} is newer than this piqueld supports ({SCHEMA_VERSION}); use a newer piqueld"
    )]
    NewerSchema(u64),
    /// The backup holds something a backup never does.
    #[error("the backup holds {0}, which isn't part of a backup")]
    UnexpectedEntry(String),
    /// The backup has no database.
    #[error("the backup holds no {DATABASE}")]
    MissingDatabase,
    /// The database isn't the one the manifest describes.
    #[error("the backup's database doesn't match its {MANIFEST}")]
    Mismatch,
    /// The data directory already holds state.
    #[error("{} is not empty; move it aside first", .0.display())]
    NotEmpty(PathBuf),
    /// A restore was interrupted, or failed and couldn't be undone.
    #[error(
        "{} holds an unfinished restore, {}; empty the data directory and run piqueld restore again",
        .0.display(), .1.display()
    )]
    Unfinished(PathBuf, PathBuf),
}

impl Backups {
    /// Restores `backup` into the data directory, which must be empty or
    /// missing, after verifying it. The database keeps the backup's schema:
    /// the daemon migrates it when it next starts. With `check_only`, the
    /// backup is verified in a temporary directory instead, and nothing
    /// else is touched.
    ///
    /// # Errors
    /// Returns a [`Refusal`] for a backup or data directory it won't
    /// restore, or the failed step. A failed restore leaves the data
    /// directory empty unless undoing it failed too, which
    /// [`Refusal::Unfinished`] reports.
    pub async fn restore(&self, backup: &Path, check_only: bool) -> Result<Manifest> {
        let manifest = Self::manifest(backup)?;
        if check_only {
            let scratch = tempfile::tempdir().context("create a temporary directory")?;
            Self::stage_restore(backup, scratch.path(), &manifest).await?;
            return Ok(manifest);
        }
        let data_dir = &self.data_dir;
        // The directories `prepare_data_dir` creates, whose entries in their
        // parents are synced once it has.
        let missing: Vec<PathBuf> = data_dir
            .ancestors()
            .take_while(|directory| fs::symlink_metadata(directory).is_err())
            .map(Path::to_path_buf)
            .collect();
        piqueld_config::directories::prepare_data_dir(data_dir)
            .await
            .with_context(|| format!("prepare {}", data_dir.display()))?;
        for directory in &missing {
            let parent = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            sync(parent.unwrap_or(Path::new(".")))?;
        }
        let _lock = piqueld_config::directories::DirectoryLock::acquire(data_dir)
            .with_context(|| format!("lock {}", data_dir.display()))?;
        if fs::read_dir(data_dir)
            .with_context(|| format!("read {}", data_dir.display()))?
            .next()
            .is_some()
        {
            bail!(Refusal::NotEmpty(data_dir.clone()));
        }
        let staging = tempfile::Builder::new()
            .prefix(STAGING)
            .tempdir_in(data_dir)
            .with_context(|| format!("create a staging directory in {}", data_dir.display()))?;
        Self::stage_restore(backup, staging.path(), &manifest).await?;
        self.publish(staging)?;
        Ok(manifest)
    }

    /// Refuses to go on while the data directory holds an unfinished
    /// restore, so the daemon never starts on partly restored state.
    ///
    /// # Errors
    /// Returns [`Refusal::Unfinished`], or the error reading the directory.
    pub fn ensure_restore_complete(&self) -> Result<()> {
        let data_dir = &self.data_dir;
        for entry in
            fs::read_dir(data_dir).with_context(|| format!("read {}", data_dir.display()))?
        {
            let entry = entry.with_context(|| format!("read {}", data_dir.display()))?;
            if entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(STAGING.as_bytes())
            {
                bail!(Refusal::Unfinished(data_dir.clone(), entry.path()));
            }
        }
        Ok(())
    }

    /// Reads `backup`'s manifest, refusing one this binary can't restore.
    fn manifest(backup: &Path) -> Result<Manifest> {
        let path = backup.join(MANIFEST);
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if manifest.format != FORMAT {
            bail!(Refusal::Format(manifest.format));
        }
        if manifest.schema_version > SCHEMA_VERSION {
            bail!(Refusal::NewerSchema(manifest.schema_version));
        }
        Ok(manifest)
    }

    /// Copies `backup` into `staging`, private and synced, then verifies it
    /// against `manifest`.
    async fn stage_restore(backup: &Path, staging: &Path, manifest: &Manifest) -> Result<()> {
        let (source, target) = (backup.to_path_buf(), staging.to_path_buf());
        tokio::task::spawn_blocking(move || {
            for entry in
                fs::read_dir(&source).with_context(|| format!("read {}", source.display()))?
            {
                let name = entry
                    .with_context(|| format!("read {}", source.display()))?
                    .file_name();
                let (from, to) = (source.join(&name), target.join(&name));
                // Neither copy follows a symlink.
                match name.to_str() {
                    Some(MANIFEST) => {}
                    Some(DATABASE | SECRET_KEY) => {
                        copy_file(CWD, &from, &from, &to)?;
                    }
                    Some(tree) if TREES.contains(&tree) => {
                        copy_tree(CWD, &from, &from, &to, None)?;
                    }
                    _ => bail!(Refusal::UnexpectedEntry(
                        name.to_string_lossy().into_owned()
                    )),
                }
            }
            sync(&target)
        })
        .await
        .context("copy the backup")??;

        let database = staging.join(DATABASE);
        if !database.exists() {
            bail!(Refusal::MissingDatabase);
        }
        let key = staging.join(SECRET_KEY);
        let identity = verify(&database, key.exists().then_some(key.as_path())).await?;
        let expected = DatabaseIdentity {
            schema_version: manifest.schema_version,
            instance_id: piqueld_core::InstanceId::parse(manifest.instance_id.clone())
                .context("parse the manifest's instance ID")?,
        };
        if identity != expected {
            bail!(Refusal::Mismatch);
        }
        Ok(())
    }

    /// Moves the verified state in `staging` into the data directory, in
    /// name order, then removes `staging`, syncing each step. `staging` is
    /// synced into the data directory first, so a crash while publishing
    /// leaves it behind as the unfinished-restore marker. If a move fails,
    /// the entries already moved go back, so the restore can run again; if
    /// that or syncing it fails, `staging` stays, marking the restore
    /// unfinished.
    pub(super) fn publish(&self, staging: tempfile::TempDir) -> Result<()> {
        let data_dir = &self.data_dir;
        sync(data_dir)?;
        let mut moved = Vec::new();
        let published = fs::read_dir(staging.path())
            .and_then(|entries| entries.map(|entry| Ok(entry?.file_name())).collect())
            .with_context(|| format!("read {}", staging.path().display()))
            .and_then(|mut names: Vec<_>| {
                names.sort();
                for name in names {
                    fs::rename(staging.path().join(&name), data_dir.join(&name)).with_context(
                        || format!("move {} into {}", name.display(), data_dir.display()),
                    )?;
                    moved.push(name);
                }
                sync(data_dir)
            });
        let Err(error) = published else {
            Self::remove_staging(staging)?;
            return sync(data_dir);
        };
        let mut stuck = Vec::new();
        for name in moved.iter().rev() {
            if let Err(undo) = fs::rename(data_dir.join(name), staging.path().join(name)) {
                stuck.push(format!("{}: {undo}", name.display()));
            }
        }
        let undone = if stuck.is_empty() {
            sync(data_dir).and_then(|()| sync(staging.path()))
        } else {
            Err(anyhow!("could not move back {}", stuck.join(", ")))
        };
        let path = staging.path().to_path_buf();
        let cleaned = match undone {
            Ok(()) => Self::remove_staging(staging),
            Err(undo) => {
                drop(staging.keep());
                Err(undo)
            }
        };
        match cleaned {
            Ok(()) => Err(error),
            Err(undo) => Err(error
                .context(format!("{undo:#}"))
                .context(Refusal::Unfinished(data_dir.clone(), path))),
        }
    }

    /// Removes the staging directory `staging`, ending the restore.
    fn remove_staging(staging: tempfile::TempDir) -> Result<()> {
        let path = staging.path().to_path_buf();
        staging
            .close()
            .with_context(|| format!("remove {}", path.display()))
    }
}
