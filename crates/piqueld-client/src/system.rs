//! Control-plane status, configuration, and readiness.
pub use piqueld_core::api::{
    BuilderCheck, BuilderStatus, BuiltOn, CertificateStatus, CheckStatus, DependencyStatus,
    DnsProviderStatus, DnsRecordState, DnsRecords, DnsStatus, HostConfiguration, ImageStatus,
    IngressStatus, InstallMethod, PACKAGE_BUILD, PrivateIngressStatus, PublicIngressStatus,
    ReadinessStatus, RegistryImageStatus, RouteStatus, SnapshotLimits, SystemCheck, SystemCheckId,
    SystemChecks, SystemStatus, TailnetStatus, TelemetryDestinationStatus, TelemetryReceiver,
    TelemetryStatus, UpdateAction, UpdateChannelRequest, UpdateRun, UpdateRunState, UpdateStatus,
};

use crate::{Client, ClientError, client::EnvelopedCall};

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.generated.system_status().data().await
    }

    /// Checks every DNS provider's credentials and zones now, returning the
    /// updated providers and certificates.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn refresh_dns(&self) -> Result<DnsStatus, ClientError> {
        self.generated.refresh_dns().data().await
    }

    /// Checks the build machine's `BuildKit` daemon now, returning that
    /// check. Fails with `builder_not_configured` when the host builds images
    /// itself.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn check_builder(&self) -> Result<BuilderCheck, ClientError> {
        self.generated.check_builder().data().await
    }

    /// Reads the release channel the installation follows, and whether it
    /// has a newer build, as of the daemon's last check.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn update_status(&self) -> Result<UpdateStatus, ClientError> {
        self.generated.update_status().data().await
    }

    /// Checks the release channel for a newer build now.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn check_for_updates(&self) -> Result<UpdateStatus, ClientError> {
        self.generated.check_for_updates().data().await
    }

    /// Follows `channel`, then checks it. Fails with
    /// `update_channel_configured` when the daemon configuration sets it.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn set_update_channel(
        &self,
        channel: crate::version::Channel,
    ) -> Result<UpdateStatus, ClientError> {
        self.generated
            .set_update_channel(&UpdateChannelRequest { channel })
            .data()
            .await
    }

    /// Asks the updater to install the channel's newer build, which restarts
    /// the daemon. Fails with `update_unavailable` where the install script
    /// didn't set piqueld up, or without a newer build.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn apply_update(&self) -> Result<UpdateStatus, ClientError> {
        self.generated.apply_update().data().await
    }

    /// Asks the updater to return to the previous build, which restarts the
    /// daemon.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn rollback_update(&self) -> Result<UpdateStatus, ClientError> {
        self.generated.rollback_update().data().await
    }

    /// Reads effective daemon configuration without changing host settings.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_configuration(&self) -> Result<HostConfiguration, ClientError> {
        self.generated.system_configuration().data().await
    }

    /// Checks the installation's health without changing it.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_checks(&self) -> Result<SystemChecks, ClientError> {
        self.generated.system_checks().data().await
    }

    /// Reads diagnostic readiness, including a structured 503 response.
    /// # Errors
    /// Returns transport or decoding errors.
    pub async fn system_readiness(&self) -> Result<ReadinessStatus, ClientError> {
        match self.generated.system_readiness().await {
            Ok(response) | Err(progenitor_client::Error::ErrorResponse(response)) => {
                Ok(response.into_inner().data)
            }
            Err(error) => Err(ClientError::from_generated(error).await),
        }
    }
}
