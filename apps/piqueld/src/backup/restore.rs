//! Restoring a backup into an empty data directory with `piqueld restore`,
//! while the daemon is stopped.
//!
//! The backup is copied into a `.staged-*` directory inside the data
//! directory and verified there: its database's integrity, its schema and
//! instance against the manifest, and its secret key against the database.
//! Only then does it become the restore's
//! [`Staging`](super::staging::Staging) directory, which
//! [`Backups::finish`] moves into place.
use super::{BackupError, Result};
use super::{
    Backups, DATABASE, FORMAT, KEPT, MANIFEST, Manifest, RolledBack, SECRET_KEY, TREES, copy_file,
    copy_tree, entries, sync, verify,
};
use piqueld_config::directories::DirectoryLock;
use piqueld_store::{DatabaseIdentity, SCHEMA_VERSION};
use rustix::fs::CWD;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

/// The name prefix of a backup's copy while it is verified, before it
/// becomes a staging directory.
const STAGED: &str = ".staged-";

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
    /// A roll-back found no backup with a schema this binary supports.
    #[error(
        "the database's schema version {schema} is newer than this piqueld supports ({SCHEMA_VERSION}), and {} holds no backup it supports; use a newer piqueld",
        .directory.display()
    )]
    NoBackup {
        /// The database's schema version.
        schema: u64,
        /// The directory of pre-migration backups.
        directory: PathBuf,
    },
    /// A roll-back's backup belongs to another instance than the database.
    #[error("the newest backup belongs to instance {0}, not the database's")]
    OtherInstance(String),
    /// A restore or roll-back stopped part way.
    #[error(
        "{} holds an unfinished restore, {}; run piqueld roll-back as the daemon's user to finish it",
        .0.display(), .1.display()
    )]
    Unfinished(PathBuf, PathBuf),
}

impl Backups {
    /// Restores `backup` into the data directory, which must be missing or
    /// hold nothing but [`Self::kept`] entries, after verifying it. The
    /// database keeps the backup's schema: the daemon migrates it when it
    /// next starts. With `check_only`, the backup is verified in a temporary
    /// directory instead, and nothing else is touched.
    ///
    /// # Errors
    /// Returns a [`Refusal`] for a backup or data directory it won't
    /// restore, or the failed step. A restore that fails once its backup is
    /// verified stays unfinished, and `piqueld roll-back` finishes it.
    pub async fn restore(&self, backup: &Path, check_only: bool) -> Result<Manifest> {
        let manifest = Self::manifest(backup)?;
        if check_only {
            let scratch = tempfile::tempdir().map_err(BackupError::TempDir)?;
            Self::stage_restore(backup, scratch.path(), &manifest).await?;
            return Ok(manifest);
        }
        let _lock = self.lock().await?;
        self.ensure_restore_complete()?;
        let data_dir = &self.data_dir;
        if !entries(data_dir, |name| !Self::kept(name))?.is_empty() {
            return Err(Refusal::NotEmpty(data_dir.clone()).into());
        }
        let staged = self.copy_verified(backup, &manifest).await?;
        let staging = self.mark(staged, piqueld_store::now_ms())?;
        self.finish(staging)?;
        Ok(manifest)
    }

    /// Whether a restore leaves the data directory's entry `name` in place:
    /// [`KEPT`] entries, and [`RolledBack`] directories.
    pub(super) fn kept(name: &OsStr) -> bool {
        RolledBack::is(name) || KEPT.iter().any(|kept| name == *kept)
    }

    /// Prepares the data directory, creating it when missing, and locks it,
    /// so no daemon runs on it while the lock is held. Removes the copies of
    /// a backup a restore stopped while verifying, which nothing uses.
    pub(super) async fn lock(&self) -> Result<DirectoryLock> {
        let data_dir = &self.data_dir;
        let missing: Vec<PathBuf> = data_dir
            .ancestors()
            .take_while(|directory| fs::symlink_metadata(directory).is_err())
            .map(Path::to_path_buf)
            .collect();
        piqueld_config::directories::prepare_data_dir(data_dir)
            .await
            .map_err(BackupError::io("prepare", data_dir))?;
        for directory in &missing {
            let parent = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            sync(parent.unwrap_or(Path::new(".")))?;
        }
        let lock = DirectoryLock::acquire(data_dir).map_err(BackupError::io("lock", data_dir))?;
        for name in entries(data_dir, |name| {
            name.as_encoded_bytes().starts_with(STAGED.as_bytes())
        })? {
            let path = data_dir.join(name);
            fs::remove_dir_all(&path).map_err(BackupError::io("remove", &path))?;
        }
        Ok(lock)
    }

    /// Copies `backup` into a new directory in the data directory and
    /// verifies it against `manifest`. It is removed if that fails.
    pub(super) async fn copy_verified(
        &self,
        backup: &Path,
        manifest: &Manifest,
    ) -> Result<tempfile::TempDir> {
        let data_dir = &self.data_dir;
        let staged = tempfile::Builder::new()
            .prefix(STAGED)
            .tempdir_in(data_dir)
            .map_err(BackupError::io("create a directory in", data_dir))?;
        Self::stage_restore(backup, staged.path(), manifest).await?;
        Ok(staged)
    }

    /// Reads `backup`'s manifest, refusing one this binary can't restore.
    pub(super) fn manifest(backup: &Path) -> Result<Manifest> {
        let path = backup.join(MANIFEST);
        let bytes = fs::read(&path).map_err(BackupError::io("read", &path))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|source| BackupError::Parse {
                path: path.clone(),
                source,
            })?;
        if manifest.format != FORMAT {
            return Err(Refusal::Format(manifest.format).into());
        }
        if manifest.schema_version > SCHEMA_VERSION {
            return Err(Refusal::NewerSchema(manifest.schema_version).into());
        }
        Ok(manifest)
    }

    /// Copies `backup` into `staging`, private and synced, then verifies it
    /// against `manifest`.
    pub(super) async fn stage_restore(
        backup: &Path,
        staging: &Path,
        manifest: &Manifest,
    ) -> Result<()> {
        let (source, target) = (backup.to_path_buf(), staging.to_path_buf());
        tokio::task::spawn_blocking(move || {
            for entry in fs::read_dir(&source).map_err(BackupError::io("read", &source))? {
                let name = entry.map_err(BackupError::io("read", &source))?.file_name();
                let (from, to) = (source.join(&name), target.join(&name));
                match name.to_str() {
                    Some(MANIFEST) => {}
                    Some(DATABASE | SECRET_KEY) => {
                        copy_file(CWD, &from, &from, &to)?;
                    }
                    Some(tree) if TREES.contains(&tree) => {
                        copy_tree(CWD, &from, &from, &to, None)?;
                    }
                    _ => {
                        return Err(
                            Refusal::UnexpectedEntry(name.to_string_lossy().into_owned()).into(),
                        );
                    }
                }
            }
            sync(&target)
        })
        .await
        .map_err(|source| BackupError::CopyTask {
            action: "copy the backup",
            source,
        })??;

        let database = staging.join(DATABASE);
        if !database.exists() {
            return Err(Refusal::MissingDatabase.into());
        }
        let key = staging.join(SECRET_KEY);
        let identity = verify(&database, key.exists().then_some(key.as_path())).await?;
        let expected = DatabaseIdentity {
            schema_version: manifest.schema_version,
            instance_id: piqueld_core::InstanceId::parse(manifest.instance_id.clone())
                .map_err(BackupError::ParseInstance)?,
        };
        if identity != expected {
            return Err(Refusal::Mismatch.into());
        }
        Ok(())
    }
}
