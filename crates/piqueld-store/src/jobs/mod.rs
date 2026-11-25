//! Durable requests to stop job runs, independent of deployment retries.
use super::{EnvironmentId, Store, StoreError};
use sqlx::SqliteConnection;

impl Store {
    /// Saves cleanup intent before attempting to stop an operation's job runs.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn request_job_cleanup(&self, operation: &str) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        Self::request_job_cleanup_on(&mut *self.connection().await?, operation).await
    }

    /// Also used inside the transaction that records a terminal job failure,
    /// so failed persistence of the earlier request cannot lose cleanup intent.
    pub(super) async fn request_job_cleanup_on(
        connection: &mut SqliteConnection,
        operation: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO job_cleanup(operation_id) VALUES(?1) ON CONFLICT DO NOTHING",
            operation
        )
        .execute(connection)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }

    /// Finds pending cleanup, including runs from superseded operations.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn pending_job_cleanup(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<String>, StoreError> {
        let environment = environment.as_str();
        sqlx::query_scalar!(
            "SELECT job_cleanup.operation_id FROM job_cleanup JOIN operations ON operations.id=job_cleanup.operation_id WHERE operations.environment_id=?1 ORDER BY job_cleanup.operation_id",
            environment
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Clears intent only after Docker confirms the job containers have stopped.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn finish_job_cleanup(&self, operation: &str) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "DELETE FROM job_cleanup WHERE operation_id=?1",
            operation
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
