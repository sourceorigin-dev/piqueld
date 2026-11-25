//! Reading a database file without opening it as a [`Store`], for
//! control-plane backups: the daemon snapshots the database before it
//! migrates it, and `piqueld restore` verifies a backup before using it.
use super::{BUSY_TIMEOUT, Store, StoreError, ensure_database_target};
use piqueld_core::InstanceId;
use sqlx::{Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;

/// One database file opened read-only, without creating, migrating or
/// changing its journal mode.
pub struct DatabaseFile(SqliteConnection);

/// The identity a migrated database records in `instance_metadata`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseIdentity {
    /// The schema version, both `PRAGMA user_version` and the one
    /// `instance_metadata` records, which agree.
    pub schema_version: u64,
    /// The control-plane instance the database belongs to.
    pub instance_id: InstanceId,
}

impl DatabaseFile {
    /// Opens the existing database at `path` read-only, refusing symlinks
    /// and special files.
    ///
    /// # Errors
    /// Returns a storage error when the file is missing or can't be opened.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        Self::connect(path, true).await
    }

    /// Opens the existing database at `path`, read-only when `read_only`,
    /// refusing symlinks and special files.
    async fn connect(path: &Path, read_only: bool) -> Result<Self, StoreError> {
        ensure_database_target(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(read_only)
            .busy_timeout(BUSY_TIMEOUT);
        SqliteConnection::connect_with(&options)
            .await
            .map(Self)
            .map_err(StoreError::database)
    }

    /// The database's schema version and instance, or `None` before its
    /// first migration. It may be newer than this binary's
    /// [`crate::SCHEMA_VERSION`].
    ///
    /// # Errors
    /// Returns [`StoreError::SchemaMismatch`] when `PRAGMA user_version` and
    /// `instance_metadata` disagree, as [`Store::open`] does, or a storage
    /// error.
    pub async fn identity(&mut self) -> Result<Option<DatabaseIdentity>, StoreError> {
        let schema_version = Store::schema_version_on(&mut self.0).await?;
        if schema_version == 0 {
            return Ok(None);
        }
        let instance_id =
            sqlx::query_scalar!("SELECT instance_id FROM instance_metadata WHERE singleton=1")
                .fetch_one(&mut self.0)
                .await
                .map_err(StoreError::database)?;
        Ok(Some(DatabaseIdentity {
            schema_version,
            instance_id: InstanceId::parse(instance_id).map_err(StoreError::corrupt)?,
        }))
    }

    /// Writes a transactionally consistent copy of the database, including
    /// commits still in its write-ahead log, to `path`, which must not exist.
    /// Safe while another process writes the database.
    ///
    /// # Errors
    /// Returns a storage error, such as when `path` exists or isn't UTF-8.
    pub async fn snapshot_into(&mut self, path: &Path) -> Result<(), StoreError> {
        let path = path.to_str().ok_or_else(|| {
            StoreError::path(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "snapshot path is not UTF-8",
            ))
        })?;
        sqlx::query("VACUUM INTO ?1")
            .bind(path)
            .execute(&mut self.0)
            .await
            .map(drop)
            .map_err(StoreError::database)
    }

    /// Checks that the master key at `key`, or no key file (`None`), is the
    /// one this database's secrets need, as the installation's secret key
    /// check does: the key must decrypt the database's verifier or, without
    /// one, a stored value. Without either, any key, or none, matches.
    ///
    /// # Errors
    /// Returns [`StoreError::SecretSource`] carrying the key's
    /// [`crate::secrets::KeyFailure`], or a storage error.
    pub async fn verify_key(&mut self, key: Option<&Path>) -> Result<(), StoreError> {
        let mut tx = self.0.begin().await.map_err(StoreError::database)?;
        Store::key_binding_on(&mut tx, key)
            .await?
            .map(drop)
            .map_err(StoreError::SecretSource)
    }

    /// Closes the connection, so the file can be moved or deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn close(self) -> Result<(), StoreError> {
        self.0.close().await.map_err(StoreError::database)
    }
}

/// A copy of a database that only this process uses, such as a backup's
/// snapshot. It opens writable, without creating the file, because SQLite
/// skips CHECK constraints in `PRAGMA integrity_check` on a read-only
/// connection, and `VACUUM INTO` copies rows without checking them. Nothing
/// here writes to it. It reads like a [`DatabaseFile`] otherwise.
pub struct DatabaseCopy(DatabaseFile);

impl DatabaseCopy {
    /// Opens the existing copy at `path`, refusing symlinks and special
    /// files.
    ///
    /// # Errors
    /// Returns a storage error when the file is missing or can't be opened.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        DatabaseFile::connect(path, false).await.map(Self)
    }

    /// The problems `PRAGMA integrity_check` finds, none for a sound
    /// database.
    ///
    /// # Errors
    /// Returns a storage error when the check can't run.
    pub async fn integrity_problems(&mut self) -> Result<Vec<String>, StoreError> {
        let mut problems: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut self.0.0)
            .await
            .map_err(StoreError::database)?;
        problems.retain(|problem| problem != "ok");
        Ok(problems)
    }

    /// Closes the connection, so the file can be moved or deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn close(self) -> Result<(), StoreError> {
        self.0.close().await
    }
}

impl std::ops::Deref for DatabaseCopy {
    type Target = DatabaseFile;

    fn deref(&self) -> &DatabaseFile {
        &self.0
    }
}

impl std::ops::DerefMut for DatabaseCopy {
    fn deref_mut(&mut self) -> &mut DatabaseFile {
        &mut self.0
    }
}

#[cfg(test)]
mod tests;
