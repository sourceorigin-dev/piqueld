//! Control-plane backups. Before the daemon migrates its database it writes the
//! state it is about to change into `<data_dir>/backups/pre-<schema>-<ms>`:
//!
//! - `piqueld.db`, a consistent snapshot that passes its integrity check;
//! - `secrets.key`, when present, proven to decrypt the snapshot's secrets;
//! - `tailscale/` and `ingress/`, the tailnet node's and gateway's state, without
//!   the gateway's runtime sockets;
//! - `backup.json`, the [`Manifest`] describing it.
//!
//! [`Backups::restore`] puts a backup back into an empty data directory, and
//! [`Backups::roll_back`] the newest one this binary opens.

mod restore;
mod roll_back;
mod rotation;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use piqueld_store::{DatabaseCopy, DatabaseFile, DatabaseIdentity, SCHEMA_VERSION};
pub use restore::Refusal;
pub use roll_back::RollBack;
use roll_back::RolledBack;
pub use rotation::PreMigration;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, OpenOptionsExt},
        },
    },
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
        if self
            .identity()
            .await?
            .is_none_or(|identity| identity.schema_version >= SCHEMA_VERSION)
        {
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
        sync(&self.data_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(PreMigration::PARTIAL)
            .tempdir_in(&directory)
            .with_context(|| format!("create a backup in {}", directory.display()))?;
        let manifest = self.stage(staging.path()).await?;
        let backup = directory.join(PreMigration::from(&manifest).name());
        fs::rename(staging.path(), &backup)
            .with_context(|| format!("move the backup to {}", backup.display()))?;
        drop(staging);
        sync(&directory)?;
        PreMigration::prune(&directory, PRE_MIGRATION_KEEP)?;
        Ok(Some(backup))
    }

    /// The identity of the data directory's database, without migrating it:
    /// `None` when there is none, or it has no schema yet.
    async fn identity(&self) -> Result<Option<DatabaseIdentity>> {
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
        Ok(identity)
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
    /// key and the state directories, all synced to disk. The manifest is
    /// written only once the snapshot passes [`verify`].
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
            let source = data_dir.join(SECRET_KEY);
            let copied = copy_file(CWD, &source, &source, &key)?;
            let control = data_dir.join(INGRESS_CONTROL);
            for tree in TREES {
                let source = data_dir.join(tree);
                copy_tree(CWD, &source, &source, &staged.join(tree), Some(&control))?;
            }
            Ok::<_, anyhow::Error>(copied.then_some(key))
        })
        .await
        .context("copy the data directory")??;

        let identity = verify(&database, key.as_deref()).await?;

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

/// A database that failed `PRAGMA integrity_check`, with the problems it
/// found. VACUUM copies rows without checking their constraints, so a
/// snapshot can hold what a later migration trips on.
#[derive(Debug, thiserror::Error)]
#[error("the database failed its integrity check: {}", .0.join("; "))]
pub struct Damaged(pub Vec<String>);

/// Checks the database copy at `database` before anything relies on it: its
/// integrity, its schema, and that the key copied to `key`, or none, is the
/// one its secrets need. Returns its identity.
async fn verify(database: &Path, key: Option<&Path>) -> Result<DatabaseIdentity> {
    let mut copy = DatabaseCopy::open(database)
        .await
        .with_context(|| format!("open {}", database.display()))?;
    let problems = copy
        .integrity_problems()
        .await
        .with_context(|| format!("check {}", database.display()))?;
    if !problems.is_empty() {
        bail!(Damaged(problems));
    }
    let identity = copy
        .identity()
        .await
        .with_context(|| format!("read the schema of {}", database.display()))?
        .context("the database has no schema yet")?;
    copy.verify_key(key)
        .await
        .with_context(|| format!("{SECRET_KEY} does not match the database"))?;
    copy.close()
        .await
        .with_context(|| format!("close {}", database.display()))?;
    Ok(identity)
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

/// Opens `name`, relative to the directory `at`, read-only and never
/// through a symlink: a directory when `directory`, else without blocking,
/// so a FIFO swapped in can't stall the copy.
fn open_at(at: BorrowedFd<'_>, name: &Path, directory: bool) -> io::Result<File> {
    let kind = if directory {
        OFlags::DIRECTORY
    } else {
        OFlags::NONBLOCK
    };
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | kind;
    Ok(File::from(rustix::fs::openat(
        at,
        name,
        flags,
        Mode::empty(),
    )?))
}

/// Copies the regular file `name` in the directory `at`, which messages
/// call `shown`, to `target`, private and synced. Returns whether it
/// existed.
fn copy_file(at: BorrowedFd<'_>, name: &Path, shown: &Path, target: &Path) -> Result<bool> {
    let mut from = match open_at(at, name, false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        opened => opened.with_context(|| format!("open {}", shown.display()))?,
    };
    let metadata = from
        .metadata()
        .with_context(|| format!("inspect {}", shown.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", shown.display());
    }
    let mut to = create_private(target)?;
    io::copy(&mut from, &mut to)
        .with_context(|| format!("copy {} to {}", shown.display(), target.display()))?;
    to.sync_all()
        .with_context(|| format!("sync {}", target.display()))?;
    Ok(true)
}

/// Copies the directory `name` in the directory `at`, which messages call
/// `shown`, to `target`, private and synced, keeping only regular files and
/// directories and leaving out `skip`. It walks open directories, so it
/// never follows a symlink, even one swapped in while it copies. Entries
/// removed meanwhile, as the gateway may, are left out.
fn copy_tree(
    at: BorrowedFd<'_>,
    name: &Path,
    shown: &Path,
    target: &Path,
    skip: Option<&Path>,
) -> Result<()> {
    let directory = match open_at(at, name, true) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        opened => opened.with_context(|| format!("open {}", shown.display()))?,
    };
    let entries =
        Dir::read_from(&directory).with_context(|| format!("read {}", shown.display()))?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(target)
        .with_context(|| format!("create {}", target.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", shown.display()))?;
        let name = Path::new(OsStr::from_bytes(entry.file_name().to_bytes()));
        if name == Path::new(".") || name == Path::new("..") {
            continue;
        }
        let (from, to) = (shown.join(name), target.join(name));
        let kind = match entry.file_type() {
            FileType::Unknown => {
                match rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Err(rustix::io::Errno::NOENT) => continue,
                    stat => FileType::from_raw_mode(
                        stat.with_context(|| format!("inspect {}", from.display()))?
                            .st_mode,
                    ),
                }
            }
            kind => kind,
        };
        match kind {
            FileType::Directory if skip != Some(from.as_path()) => {
                copy_tree(directory.as_fd(), name, &from, &to, skip)?;
            }
            FileType::RegularFile => {
                copy_file(directory.as_fd(), name, &from, &to)?;
            }
            _ => {}
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

/// The names of the entries in the directory `path` that `keep` keeps,
/// sorted.
fn entries(path: &Path, keep: impl Fn(&OsStr) -> bool) -> Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let name = entry
            .with_context(|| format!("read {}", path.display()))?
            .file_name();
        if keep(&name) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// A failed [`move_entries`].
struct Moved {
    /// The move that failed.
    error: anyhow::Error,
    /// Why the entries already moved didn't all go back, if they didn't.
    undo: Option<anyhow::Error>,
}

/// Moves the entries `names` of the directory `from` into the directory
/// `to`, on the same filesystem, in order, then syncs both. If a move
/// fails, the entries already moved go back, so either all move or none.
fn move_entries(from: &Path, to: &Path, names: &[OsString]) -> Result<(), Moved> {
    let mut moved = Vec::new();
    let forward = names
        .iter()
        .try_for_each(|name| {
            fs::rename(from.join(name), to.join(name)).with_context(|| {
                format!("move {} into {}", from.join(name).display(), to.display())
            })?;
            moved.push(name);
            Ok(())
        })
        .and_then(|()| sync(to))
        .and_then(|()| sync(from));
    let Err(error) = forward else {
        return Ok(());
    };
    let stuck: Vec<_> = moved
        .iter()
        .rev()
        .filter_map(|name| {
            fs::rename(to.join(name), from.join(name))
                .err()
                .map(|undo| format!("{}: {undo}", name.display()))
        })
        .collect();
    let undo = if stuck.is_empty() {
        sync(to).and_then(|()| sync(from)).err()
    } else {
        Some(anyhow::anyhow!(
            "could not move back into {} {}",
            from.display(),
            stuck.join(", ")
        ))
    };
    Err(Moved { error, undo })
}
