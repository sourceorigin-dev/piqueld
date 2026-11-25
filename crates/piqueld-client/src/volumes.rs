//! Volumes: the Docker volumes the daemon tracks, and the names environments
//! bind to them.
pub use piqueld_core::api::{
    AttachVolumeRequest, DetachedFrom, VolumeBinding, VolumePool, VolumeState, VolumeView,
};
pub use piqueld_core::{DockerVolumeName, VolumeId, VolumeName};

use crate::{Client, ClientError, client::EnvelopedCall};

impl Client {
    /// Lists every volume, or only detached ones.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn volumes(&self, detached: bool) -> Result<Vec<VolumeView>, ClientError> {
        self.generated.list_volumes(Some(detached)).data().await
    }

    /// Fetches one volume.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn volume(&self, id: &str) -> Result<VolumeView, ClientError> {
        self.generated.get_volume(id).data().await
    }

    /// Lists the volumes an environment binds its volume names to.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment_volumes(
        &self,
        environment: &str,
    ) -> Result<Vec<VolumeView>, ClientError> {
        self.generated
            .list_environment_volumes(environment)
            .data()
            .await
    }

    /// Attaches a detached volume to an environment under `name`.
    ///
    /// # Errors
    /// Returns [`ClientError`], including `volume_name_bound`,
    /// `volume_attached` and `volume_preview` refusals.
    pub async fn attach_volume(
        &self,
        environment: &str,
        name: &str,
        request: &AttachVolumeRequest,
    ) -> Result<VolumeView, ClientError> {
        self.generated
            .attach_volume(environment, name, request)
            .data()
            .await
    }

    /// Deletes a detached volume and its data.
    ///
    /// # Errors
    /// Returns [`ClientError`], including `volume_attached`.
    pub async fn delete_volume(&self, id: &str) -> Result<(), ClientError> {
        self.generated
            .delete_volume(id)
            .data()
            .await
            .map(|_: bool| ())
    }
}
