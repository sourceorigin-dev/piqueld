//! The volume survey: adopting the Docker volumes the daemon manages but
//! has no record of, and measuring every volume's size.
use super::{Controller, DockerApi, OperationError};
use crate::ticker::Ticker;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Time between volume surveys. Measuring walks every volume's files, so it
/// runs rarely.
const SURVEY_PERIOD: Duration = Duration::from_mins(15);

impl<D: DockerApi> Controller<D> {
    /// Surveys volumes, measuring them, every [`SURVEY_PERIOD`] until
    /// cancelled, which also stops a survey in progress. Failures are
    /// reported as daemon diagnostics.
    pub async fn run_volume_survey(&self, cancellation: CancellationToken) {
        let mut ticks = Ticker::new(SURVEY_PERIOD);
        while ticks.tick(&cancellation).await {
            let surveyed = tokio::select! {
                () = cancellation.cancelled() => return,
                surveyed = self.survey_volumes(true) => surveyed,
            };
            if let Err(error) = surveyed {
                tracing::warn!(%error, "volume survey failed");
                self.store
                    .report_diagnostic(&error.diagnostic(), None)
                    .await;
            }
        }
    }

    /// Lists every volume the instance manages and records them (see
    /// `Store::record_volume_survey`), then, when `measure`, measures and
    /// records their sizes; a failed measurement keeps the sizes recorded
    /// before. Each deployment surveys, unmeasured, before saving its
    /// target, so a name binds to the old volume the survey adopts rather
    /// than a new one.
    ///
    /// # Errors
    /// Returns Docker errors listing volumes, or storage errors.
    pub async fn survey_volumes(&self, measure: bool) -> Result<(), OperationError> {
        let listed_at = piqueld_store::now_ms();
        let volumes = self.docker.volumes(self.store.instance_id()).await?;
        self.store.record_volume_survey(&volumes, listed_at).await?;
        if measure {
            match self.docker.volume_sizes().await {
                Ok(sizes) => self.store.record_volume_sizes(&sizes).await?,
                Err(error) => tracing::warn!(%error, "measuring volumes failed; sizes kept"),
            }
        }
        Ok(())
    }
}
