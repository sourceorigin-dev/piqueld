//! Control-plane backups. Migrations only go forward, so before the daemon
//! migrates its database it writes the state it is about to change into
//! `<data_dir>/backups/pre-<schema>-<ms>`, a directory holding:
//!
//! - `piqueld.db`, a consistent snapshot of the database;
//! - `secrets.key`, when present, proven to decrypt the snapshot's secrets;
//! - `tailscale/` and `ingress/`, the tailnet node's and the ingress
//!   gateway's state, as regular files and directories, without the
//!   gateway's runtime sockets in `ingress/control`;
//! - `backup.json`, the [`Manifest`] describing it.
//!
//! Everything else in the data directory is left out: caches recreated on
//! start, local-preview bundles in `snapshots/`, retired keys, and earlier
//! backups.

mod rotation;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use piqueld_store::{DatabaseFile, SCHEMA_VERSION};
pub use rotation::PreMigration;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// The backup layout [`Manifest::format`] names.
const FORMAT: u32 = 1;
/// The manifest's file in a backup.
const MANIFEST: &str = "backup.json";
/// The database, in the data directory and in a backup.
const DATABASE: &str = "piqueld.db";
/// The secret master key, beside the database.
const SECRET_KEY: &str = "secrets.key";
/// Directories copied whole: the tailnet node's and the ingress gateway's.
const TREES: [&str; 2] = ["tailscale", "ingress"];
/// The gateway's runtime sockets, left out of `ingress/`.
const INGRESS_CONTROL: &str = "ingress/control";
/// The data directory's subdirectory holding pre-migration backups.
const BACKUPS: &str = "backups";
/// How many pre-migration backups the daemon keeps.
const PRE_MIGRATION_KEEP: usize = 3;

/// What a backup holds, written into it as `backup.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Manifest {
    /// The backup's layout version.
    pub format: u32,
    /// The schema version of the backed-up database.
    pub schema_version: u64,
    /// The version of the daemon that wrote the backup.
    pub daemon_version: String,
    /// The control-plane instance the database belongs to.
    pub instance_id: String,
    /// When the backup was taken, in Unix milliseconds.
    pub created_at_ms: i64,
}

/// The backups of one data directory.
pub struct Backups {
    /// The daemon's data directory, `server.data_dir`.
    data_dir: PathBuf,
}

impl Backups {
    /// The backups of the data directory `data_dir`.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// The directory holding pre-migration backups.
    fn directory(&self) -> PathBuf {
        self.data_dir.join(BACKUPS)
    }

    /// Backs up the data directory when its database is older than this
    /// binary, so the daemon is about to migrate it, keeping the newest
    /// [`PRE_MIGRATION_KEEP`] such backups. Call it with the data directory
    /// locked, before anything writes to it. Returns the new backup's path.
    ///
    /// # Errors
    /// Returns the failed step. A failed backup leaves nothing behind.
    pub async fn before_migration(&self) -> Result<Option<PathBuf>> {
        let database = self.data_dir.join(DATABASE);
        if !database
            .try_exists()
            .with_context(|| format!("inspect {}", database.display()))?
        {
            return Ok(None);
        }
        let mut live = DatabaseFile::open(&database)
            .await
            .with_context(|| format!("open {}", database.display()))?;
        let identity = live
            .identity()
            .await
            .context("read the database's schema")?;
        live.close().await.context("close the database")?;
        // A new database has nothing to lose, and the store refuses a newer
        // one.
        if identity.is_none_or(|identity| identity.schema_version >= SCHEMA_VERSION) {
            return Ok(None);
        }

        let directory = self.directory();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .or_else(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => Ok(()),
                _ => Err(error),
            })
            .with_context(|| format!("create {}", directory.display()))?;
        // Written under a temporary name, so an unfinished backup never looks
        // like a finished one, and is removed if it fails.
        let staging = tempfile::Builder::new()
            .prefix(PreMigration::PARTIAL)
            .tempdir_in(&directory)
            .with_context(|| format!("create a backup in {}", directory.display()))?;
        let manifest = self.stage(staging.path()).await?;
        let backup = directory.join(PreMigration::from(&manifest).name());
        fs::rename(staging.path(), &backup)
            .with_context(|| format!("move the backup to {}", backup.display()))?;
        // Moved, so dropping the staging directory removes nothing.
        drop(staging);
        sync(&directory)?;
        PreMigration::prune(&directory, PRE_MIGRATION_KEEP)?;
        Ok(Some(backup))
    }

    /// The newest pre-migration backup, if any.
    ///
    /// # Errors
    /// Returns the error listing the backups directory.
    pub fn latest_before_migration(&self) -> Result<Option<(PathBuf, PreMigration)>> {
        let directory = self.directory();
        Ok(PreMigration::list(&directory)?
            .into_iter()
            .next()
            .map(|backup| (directory.join(backup.name()), backup)))
    }

    /// Writes a backup of the data directory into `destination`, an empty
    /// private directory: a snapshot of the database first, then the secret
    /// key checked against it, the state directories and the manifest, all
    /// synced to disk.
    async fn stage(&self, destination: &Path) -> Result<Manifest> {
        let database = destination.join(DATABASE);
        let mut live = DatabaseFile::open(&self.data_dir.join(DATABASE))
            .await
            .context("open the database")?;
        live.snapshot_into(&database)
            .await
            .context("snapshot the database")?;
        live.close().await.context("close the database")?;

        let data_dir = self.data_dir.clone();
        let staged = destination.to_path_buf();
        let key = tokio::task::spawn_blocking(move || {
            let key = staged.join(SECRET_KEY);
            let copied = copy_file(&data_dir.join(SECRET_KEY), &key)?;
            for tree in TREES {
                let control = data_dir.join(INGRESS_CONTROL);
                copy_tree(&data_dir.join(tree), &staged.join(tree), Some(&control))?;
            }
            Ok::<_, anyhow::Error>(copied.then_some(key))
        })
        .await
        .context("copy the data directory")??;

        let mut snapshot = DatabaseFile::open(&database)
            .await
            .context("open the database snapshot")?;
        let identity = snapshot
            .identity()
            .await
            .context("read the snapshot's schema")?
            .context("the database has no schema yet")?;
        snapshot
            .verify_key(key.as_deref())
            .await
            .with_context(|| format!("{SECRET_KEY} does not match the database"))?;
        snapshot
            .close()
            .await
            .context("close the database snapshot")?;

        let manifest = Manifest {
            format: FORMAT,
            schema_version: identity.schema_version,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_id: identity.instance_id.as_str().to_owned(),
            created_at_ms: piqueld_store::now_ms(),
        };
        let path = destination.join(MANIFEST);
        let mut file = create_private(&path)?;
        serde_json::to_writer_pretty(&mut file, &manifest)
            .with_context(|| format!("write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        sync(destination)?;
        Ok(manifest)
    }
}

/// Creates the private file `path`, which must not exist.
fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

/// Copies the regular file `source` to `target`, private and synced,
/// without following a symlink. Returns whether `source` existed.
fn copy_file(source: &Path, target: &Path) -> Result<bool> {
    let mut from = match OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(source)
    {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        opened => opened.with_context(|| format!("open {}", source.display()))?,
    };
    let mut to = create_private(target)?;
    io::copy(&mut from, &mut to)
        .with_context(|| format!("copy {} to {}", source.display(), target.display()))?;
    to.sync_all()
        .with_context(|| format!("sync {}", target.display()))?;
    Ok(true)
}

/// Copies the directory `source` to `target`, private and synced, keeping
/// only regular files and directories and leaving out `skip`. Entries
/// removed while it copies, as the gateway may, are left out too.
fn copy_tree(source: &Path, target: &Path, skip: Option<&Path>) -> Result<()> {
    let entries = match fs::read_dir(source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries.with_context(|| format!("read {}", source.display()))?,
    };
    fs::DirBuilder::new()
        .mode(0o700)
        .create(target)
        .with_context(|| format!("create {}", target.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", source.display()))?;
        let (from, to) = (entry.path(), target.join(entry.file_name()));
        let kind = match entry.file_type() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            kind => kind.with_context(|| format!("inspect {}", from.display()))?,
        };
        if kind.is_dir() && skip != Some(from.as_path()) {
            copy_tree(&from, &to, skip)?;
        } else if kind.is_file() {
            copy_file(&from, &to)?;
        }
    }
    sync(target)
}

/// Syncs the directory `path`'s entries to disk.
fn sync(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync {}", path.display()))
}
