//! The volume survey: adopting the Docker volumes the daemon manages but
//! has no record of, and measuring every volume's size.
use super::{Controller, DockerApi, OperationError};
use crate::ticker::Ticker;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Time between volume surveys. Measuring walks every volume's files, so it
/// runs rarely; the first survey runs at startup.
const SURVEY_PERIOD: Duration = Duration::from_mins(15);

impl<D: DockerApi> Controller<D> {
    /// Surveys volumes every [`SURVEY_PERIOD`] until cancelled. Failures are
    /// reported as daemon diagnostics.
    pub async fn run_volume_survey(&self, cancellation: CancellationToken) {
        let mut ticks = Ticker::new(SURVEY_PERIOD);
        while ticks.tick(&cancellation).await {
            if let Err(error) = self.survey_volumes().await {
                tracing::warn!(%error, "volume survey failed");
                self.store
                    .report_diagnostic(&error.diagnostic(), None)
                    .await;
            }
        }
    }

    /// Lists every volume the instance manages, with its size, and records
    /// them (see `Store::record_volume_survey`).
    ///
    /// # Errors
    /// Returns Docker or storage errors.
    pub async fn survey_volumes(&self) -> Result<(), OperationError> {
        let volumes = self.docker.volumes(self.store.instance_id()).await?;
        Ok(self.store.record_volume_survey(&volumes).await?)
    }
}
