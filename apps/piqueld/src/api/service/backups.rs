//! Backup repositories: creating one on disk and recording it, checking it,
//! and removing it from piqueld.
use super::{ApplicationError, ApplicationService, Author, admission::Expensive};
use crate::backup::RepositoryAccess;
use piqueld_core::backup::{
    BackupRepository, BackupRepositoryName, RecoveryKit, RepositoryCheck, RepositoryRequest,
};

impl ApplicationService {
    /// Lists backup repositories by name.
    ///
    /// # Errors
    /// Returns storage failures.
    pub async fn backup_repositories(&self) -> Result<Vec<BackupRepository>, ApplicationError> {
        Ok(self.store.backup_repositories().await?)
    }

    /// Creates the repository `name` with a daemon key and a recovery key,
    /// then records it with the daemon key; returns the recovery key, which
    /// nothing keeps. A repository the store then refuses is removed again.
    ///
    /// # Errors
    /// Returns busy, refusal, a taken name or location, a directory holding
    /// files, the repository's own failure, or storage failures.
    pub async fn create_backup_repository(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
        request: RepositoryRequest,
    ) -> Result<RecoveryKit, ApplicationError> {
        let _admitted = self.admit(Expensive::Backups)?;
        let location = request.location;
        self.store
            .check_new_backup_repository(name, &location)
            .await?;
        let daemon_key = RepositoryAccess::generate_key()?;
        let recovery_key = RepositoryAccess::generate_key()?;
        let (key, recovery, at) = (daemon_key.clone(), recovery_key.clone(), location.clone());
        tokio::task::spawn_blocking(move || RepositoryAccess::new(&at, &key).create(&recovery))
            .await
            .map_err(crate::backup::EngineError::from)??;
        match self
            .store
            .add_backup_repository(author, name, &location, &daemon_key)
            .await
        {
            Ok(repository) => Ok(RecoveryKit {
                repository,
                recovery_key: recovery_key.to_string(),
            }),
            Err(error) => {
                let discard = move || RepositoryAccess::new(&location, &daemon_key).discard();
                if let Err(join) = tokio::task::spawn_blocking(discard).await {
                    tracing::warn!(error = %join, "could not remove a repository the store refused");
                }
                Err(error.into())
            }
        }
    }

    /// Checks the structure of the repository `name` and records the result;
    /// one that can't be opened, such as after a lost key, fails the check.
    ///
    /// # Errors
    /// Returns busy, refusal, absence, secret storage or storage failures.
    pub async fn check_backup_repository(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
    ) -> Result<BackupRepository, ApplicationError> {
        let _admitted = self.admit(Expensive::Backups)?;
        let (repository, key) = self.store.backup_repository_key(name).await?;
        let location = repository.location.clone();
        let checked =
            tokio::task::spawn_blocking(move || RepositoryAccess::new(&location, &key).check())
                .await
                .map_err(crate::backup::EngineError::from)?;
        let problem = checked.unwrap_or_else(|error| Some(error.to_string()));
        let check = RepositoryCheck {
            checked_at_ms: piqueld_store::now_ms(),
            problem,
        };
        self.store
            .record_backup_repository_check(author, name, &check)
            .await?;
        Ok(BackupRepository {
            last_check: Some(check),
            ..repository
        })
    }

    /// Removes the repository `name` from piqueld, keeping its backups.
    ///
    /// # Errors
    /// Returns refusal, absence or storage failures.
    pub async fn delete_backup_repository(
        &self,
        author: impl Into<Author<'_>>,
        name: &BackupRepositoryName,
    ) -> Result<(), ApplicationError> {
        Ok(self.store.delete_backup_repository(author, name).await?)
    }
}
