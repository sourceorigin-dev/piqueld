//! Backup repositories: where they are and the key piqueld opens them with,
//! encrypted with the secret master key. The repositories themselves, and the
//! backups in them, live outside the database.
use crate::secrets::{Envelope, SecretCipher, SecretOwner};
use crate::{Author, SecurityEvent, Store, StoreError, new_id, now_ms};
use piqueld_core::access::{GlobalPermission, Permission};
use piqueld_core::backup::{
    BackupRepository, BackupRepositoryName, RepositoryCheck, RepositoryLocation, RepositoryPath,
};
use sqlx::{Sqlite, Transaction};
use zeroize::Zeroizing;

/// A repository change refused before anything is stored.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// Another repository has the name.
    #[error("a backup repository is already named {0}")]
    NameTaken(BackupRepositoryName),
    /// Another repository is at the location.
    #[error("backup repository {0} is already there")]
    LocationTaken(BackupRepositoryName),
}

/// The permission every repository change needs.
const MANAGE: Permission = Permission::Global(GlobalPermission::BackupsManage);

/// The stored columns a location is kept as: its kind and where it is.
fn location_columns(location: &RepositoryLocation) -> (&'static str, &str) {
    match location {
        RepositoryLocation::Local { path } => ("local", path.as_str()),
    }
}

/// A row as stored, without its key.
struct RepositoryRow {
    name: String,
    kind: String,
    location: String,
    created_at_ms: i64,
    checked_at_ms: Option<i64>,
    check_problem: Option<String>,
}

impl TryFrom<RepositoryRow> for BackupRepository {
    type Error = StoreError;

    fn try_from(row: RepositoryRow) -> Result<Self, StoreError> {
        let location = match row.kind.as_str() {
            "local" => RepositoryLocation::Local {
                path: RepositoryPath::parse(row.location).map_err(StoreError::corrupt)?,
            },
            _ => return Err(StoreError::Corrupt),
        };
        Ok(Self {
            name: BackupRepositoryName::parse(row.name).map_err(StoreError::corrupt)?,
            location,
            created_at_ms: row.created_at_ms,
            last_check: row.checked_at_ms.map(|checked_at_ms| RepositoryCheck {
                checked_at_ms,
                problem: row.check_problem,
            }),
        })
    }
}

impl Store {
    /// Lists every backup repository by name.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn backup_repositories(&self) -> Result<Vec<BackupRepository>, StoreError> {
        sqlx::query_as!(
            RepositoryRow,
            "SELECT name,kind,location,created_at_ms,checked_at_ms,check_problem
            FROM backup_repositories ORDER BY name"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(BackupRepository::try_from)
        .collect()
    }

    /// The backup repository `name`.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] or storage errors.
    pub async fn backup_repository(
        &self,
        name: &BackupRepositoryName,
    ) -> Result<BackupRepository, StoreError> {
        let name = name.as_str();
        sqlx::query_as!(
            RepositoryRow,
            "SELECT name,kind,location,created_at_ms,checked_at_ms,check_problem
            FROM backup_repositories WHERE name=?1",
            name,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?
        .try_into()
    }

    /// Refuses a new repository whose name or location another one has, so
    /// nothing is created on disk for a request the store would refuse.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] or storage errors.
    pub async fn check_new_backup_repository(
        &self,
        name: &BackupRepositoryName,
        location: &RepositoryLocation,
    ) -> Result<(), StoreError> {
        let mut connection = self.pool.acquire().await.map_err(StoreError::database)?;
        Self::check_new_repository_on(&mut connection, name, location).await
    }

    /// See [`Self::check_new_backup_repository`].
    async fn check_new_repository_on(
        db: &mut sqlx::SqliteConnection,
        name: &BackupRepositoryName,
        location: &RepositoryLocation,
    ) -> Result<(), StoreError> {
        let (kind, location) = location_columns(location);
        let name_str = name.as_str();
        let taken = sqlx::query!(
            "SELECT name FROM backup_repositories WHERE name=?1 OR (kind=?2 AND location=?3)",
            name_str,
            kind,
            location,
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?;
        match taken {
            None => Ok(()),
            Some(row) if row.name == name_str => {
                Err(RepositoryError::NameTaken(name.clone()).into())
            }
            Some(row) => Err(RepositoryError::LocationTaken(
                BackupRepositoryName::parse(row.name).map_err(StoreError::corrupt)?,
            )
            .into()),
        }
    }

    /// Records the repository `name`, just created at `location`, with the
    /// key piqueld opens it with, encrypted. The author's actor needs
    /// `backups:manage` and a recent passkey sign-in, since the response
    /// shows the recovery key; both are checked in the transaction, which
    /// also records the change and a `backup_repository_changed` security
    /// event, never a key.
    ///
    /// # Errors
    /// Returns refusal, [`RepositoryError`], secret storage or database errors.
    pub async fn add_backup_repository(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
        location: &RepositoryLocation,
        key: &Zeroizing<String>,
    ) -> Result<BackupRepository, StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor.require_on(&mut tx, MANAGE, None).await?;
        actor.require_fresh(&mut tx).await?;
        Self::check_new_repository_on(&mut tx, name, location).await?;
        let id = new_id("brp");
        let envelope = self
            .verified_secret_cipher(&mut tx)
            .await?
            .encrypt(SecretOwner::BackupRepository(&id), "key", 1, key.as_bytes())
            .map_err(StoreError::SecretSource)?;
        let created_at_ms = now_ms();
        let (kind, path) = location_columns(location);
        let name_str = name.as_str();
        sqlx::query!(
            "INSERT INTO backup_repositories(id,name,kind,location,nonce,ciphertext,created_at_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7)",
            id,
            name_str,
            kind,
            path,
            envelope.nonce,
            envelope.ciphertext,
            created_at_ms,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let message = format!("Backup repository {name} was created at {path}");
        Self::repository_changed(&mut tx, actor, &message).await?;
        let change = format!("backup repository {name_str:?} {id} created at {kind} {path:?}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        Ok(BackupRepository {
            name: name.clone(),
            location: location.clone(),
            created_at_ms,
            last_check: None,
        })
    }

    /// Removes the repository `name` from piqueld, leaving it and its
    /// backups where they are. The author's actor needs `backups:manage`,
    /// checked in the transaction, which also records the change and a
    /// `backup_repository_changed` security event.
    ///
    /// # Errors
    /// Returns refusal, absence or database errors.
    pub async fn delete_backup_repository(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor.require_on(&mut tx, MANAGE, None).await?;
        let name_str = name.as_str();
        let id = sqlx::query_scalar!(
            "DELETE FROM backup_repositories WHERE name=?1 RETURNING id AS \"id!\"",
            name_str,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let message = format!("Backup repository {name} was removed; its backups were kept");
        Self::repository_changed(&mut tx, actor, &message).await?;
        let change = format!("backup repository {name_str:?} {id} removed");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        Ok(())
    }

    /// Records a `backup_repository_changed` security event.
    async fn repository_changed(
        tx: &mut Transaction<'_, Sqlite>,
        actor: crate::Actor<'_>,
        message: &str,
    ) -> Result<(), StoreError> {
        let event = SecurityEvent::BackupRepositoryChanged;
        Self::security_event_on(tx, event, message, actor.attribution()).await
    }

    /// The repository `name` and the key piqueld opens it with.
    ///
    /// # Errors
    /// Returns absence, secret storage errors, such as a lost master key, or
    /// database errors.
    pub async fn backup_repository_key(
        &self,
        name: &BackupRepositoryName,
    ) -> Result<(BackupRepository, Zeroizing<String>), StoreError> {
        let name_str = name.as_str();
        let row = sqlx::query!(
            "SELECT id AS \"id!\",nonce,ciphertext FROM backup_repositories WHERE name=?1",
            name_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let envelope = Envelope {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        let key = SecretCipher::load(&self.secret_key_path, true)
            .and_then(|cipher| {
                cipher.decrypt(SecretOwner::BackupRepository(&row.id), "key", 1, &envelope)
            })
            .map_err(StoreError::SecretSource)?;
        let key = String::from_utf8(key.to_vec())
            .map(Zeroizing::new)
            .map_err(StoreError::corrupt)?;
        Ok((self.backup_repository(name).await?, key))
    }

    /// Records `check` as the latest of the repository `name`. The author's
    /// actor needs `backups:manage`, checked in the transaction, which also
    /// records the check's outcome.
    ///
    /// # Errors
    /// Returns refusal, absence, for a repository removed meanwhile, or
    /// storage errors.
    pub async fn record_backup_repository_check(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
        check: &RepositoryCheck,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        author.actor.require_on(&mut tx, MANAGE, None).await?;
        let name_str = name.as_str();
        let id = sqlx::query_scalar!(
            "UPDATE backup_repositories SET checked_at_ms=?2,check_problem=?3 WHERE name=?1
            RETURNING id AS \"id!\"",
            name_str,
            check.checked_at_ms,
            check.problem,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let outcome = if check.problem.is_some() {
            "failed"
        } else {
            "passed"
        };
        let change = format!("backup repository {name_str:?} {id} check {outcome}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }
}
