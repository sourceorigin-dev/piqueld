//! Control-plane status, configuration, and readiness.
pub use piqueld_core::api::{
    BuilderCheck, BuilderStatus, BuiltOn, CertificateStatus, CheckStatus, DependencyStatus,
    DnsProviderStatus, DnsRecordState, DnsRecords, DnsStatus, HostConfiguration, ImageStatus,
    IngressStatus, PrivateIngressStatus, PublicIngressStatus, ReadinessStatus, RegistryImageStatus,
    RouteStatus, Setting, SettingChange, SettingKind, SettingSource, SnapshotLimits, SystemCheck,
    SystemCheckId, SystemChecks, SystemStatus, TailnetStatus, TelemetryDestinationStatus,
    TelemetryReceiver, TelemetryStatus,
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

    /// Reads the daemon's settings: the host's, and every daemon setting
    /// with its effective value.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_configuration(&self) -> Result<HostConfiguration, ClientError> {
        self.generated.system_configuration().data().await
    }

    /// Sets the daemon setting `key` to `value`, which applies at once.
    /// # Errors
    /// Returns transport, decoding or API errors, including
    /// `setting_unknown`, `setting_pinned` and `setting_invalid`.
    pub async fn put_setting(
        &self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<Setting, ClientError> {
        self.generated
            .put_setting(key, &SettingChange { value })
            .data()
            .await
    }

    /// Resets the daemon setting `key` to its default, which applies at once.
    /// # Errors
    /// Returns transport, decoding or API errors, including
    /// `setting_unknown` and `setting_pinned`.
    pub async fn reset_setting(&self, key: &str) -> Result<Setting, ClientError> {
        self.generated.reset_setting(key).data().await
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
