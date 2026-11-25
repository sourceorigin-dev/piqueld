//! Backup repositories. It re-exports `piqueld_core::backup`.
use crate::{
    Client, ClientError,
    client::{EnvelopedCall, GeneratedCall},
};
pub use piqueld_core::backup::*;

impl Client {
    /// Lists backup repositories, without their keys.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn backup_repositories(&self) -> Result<Vec<BackupRepository>, ClientError> {
        self.generated.backup_repositories().data().await
    }
    /// Creates the backup repository `name`; the result holds its recovery
    /// key, which is shown only once.
    /// # Errors
    /// Returns transport, refusal, decoding or API errors.
    pub async fn create_backup_repository(
        &self,
        name: &str,
        request: &RepositoryRequest,
    ) -> Result<RecoveryKit, ClientError> {
        self.generated
            .create_backup_repository(name, request)
            .data()
            .await
    }
    /// Checks the backup repository `name`'s structure and records the
    /// result.
    /// # Errors
    /// Returns transport, absence, decoding or API errors.
    pub async fn check_backup_repository(
        &self,
        name: &str,
    ) -> Result<BackupRepository, ClientError> {
        self.generated.check_backup_repository(name).data().await
    }
    /// Removes the backup repository `name` from piqueld, keeping its backups.
    /// # Errors
    /// Returns transport, absence, decoding or API errors.
    pub async fn delete_backup_repository(&self, name: &str) -> Result<(), ClientError> {
        self.generated
            .delete_backup_repository(name)
            .payload()
            .await
            .map(|_| ())
    }
}
