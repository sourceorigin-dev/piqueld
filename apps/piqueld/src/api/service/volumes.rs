//! Volumes: listing the volumes the daemon tracks, attaching detached ones
//! to environments, and deleting detached ones with their data.
use super::{ApplicationError, ApplicationService, Author};
use crate::application::BoundaryError;
use piqueld_core::api::VolumeView;
use piqueld_core::{EnvironmentId, Phase, VolumeId, VolumeName};

impl ApplicationService {
    /// Every volume, or only detached ones.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn volumes(&self, detached: bool) -> Result<Vec<VolumeView>, ApplicationError> {
        Ok(self.store.volumes(detached).await?)
    }

    /// The volumes `environment` binds its volume names to.
    ///
    /// # Errors
    /// Returns storage errors, `NotFound` for a missing environment.
    pub async fn environment_volumes(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<VolumeView>, ApplicationError> {
        self.store.get(environment).await?;
        Ok(self.store.environment_volumes(environment).await?)
    }

    /// One volume.
    ///
    /// # Errors
    /// Returns storage errors, `NotFound` for a missing volume.
    pub async fn volume(&self, id: &VolumeId) -> Result<VolumeView, ApplicationError> {
        Ok(self.store.volume(id).await?)
    }

    /// Binds `name` of `environment` to the detached volume `volume`; see
    /// `Store::attach_volume`.
    ///
    /// # Errors
    /// Returns the store's refusals and storage errors.
    pub async fn attach_volume(
        &self,
        author: impl Into<Author<'_>>,
        environment: &EnvironmentId,
        name: &VolumeName,
        volume: &VolumeId,
    ) -> Result<VolumeView, ApplicationError> {
        Ok(self
            .store
            .attach_volume(author, environment, name, volume)
            .await?)
    }

    /// Deletes the detached volume `id` and its data: marks it removing,
    /// removes it from Docker as a journaled `remove_volumes` action, then
    /// forgets it. If Docker fails, the volume is kept, so deleting it again
    /// retries.
    ///
    /// # Errors
    /// Returns `Attached` for an attached volume, storage errors, or the
    /// Docker removal's error.
    pub async fn delete_volume(
        &self,
        author: impl Into<Author<'_>>,
        id: &VolumeId,
    ) -> Result<(), ApplicationError> {
        let volume = self.store.delete_volume(author, id).await?;
        let journal = self
            .store
            .begin_action(
                None,
                Phase::RemoveVolumes,
                Some(volume.docker_name.as_str()),
            )
            .await?;
        let result = match self.store.action_request(&journal, 1).await {
            Ok(()) => self.runtime.remove_volume(&volume).await,
            Err(error) => Err(error.into()),
        };
        self.store
            .finish_volume_removal(&volume.id, result.is_ok())
            .await?;
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(BoundaryError::diagnostic),
            )
            .await?;
        Ok(result?)
    }
}
