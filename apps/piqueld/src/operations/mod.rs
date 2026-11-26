//! Errors encountered while reconciling application operations, and the
//! diagnostics recorded for them.
use piqueld_config::unix_http::UnixHttpError;
use piqueld_core::observability::{DeploymentFailure, DiagnosticCode};
use piqueld_docker::command::CommandError;
use piqueld_store::OperationFailure;

/// A fresh, time-ordered ID for a recorded diagnostic, e.g.
/// `diagnostic-0190c4f6a3e27b8a9d1e4f6a3e27b8a9`.
pub(crate) fn diagnostic_id() -> String {
    format!("diagnostic-{}", uuid::Uuid::now_v7().simple())
}

/// Sanitized failure returned while executing a durable operation.
#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    /// A saved or captured route belongs to another environment or the installation.
    #[error("{0}")]
    HostnameConflict(#[source] piqueld_store::StoreError),
    /// Managed gateway could not apply the required routing transition.
    #[error("ingress configuration could not be applied; see ingress health and daemon logs")]
    Ingress(#[source] anyhow::Error),
    /// Docker failure with its complete diagnostic cause chain.
    #[error("{}", OperationError::docker_classification(.0))]
    Docker(#[source] piqueld_docker::DockerError),
    /// Durable operation failure with its storage cause.
    #[error("operation journal is unavailable")]
    Journal(#[source] piqueld_store::StoreError),
    /// Secret values could not be decrypted with the configured master key.
    #[error("secret storage is unavailable")]
    SecretStorageUnavailable(#[source] piqueld_store::StoreError),
    /// Logical names whose values must be replaced before a new deployment.
    #[error(
        "secret values unavailable: {names}; supply replacement values and start a new deployment"
    )]
    SecretUnavailable {
        /// Logical names only, never values.
        names: String,
    },
    /// Mounted secrets have no value to deploy.
    #[error("{0}")]
    SecretMissing(#[source] piqueld_store::StoreError),
    /// The environment mounts a stored secret its access list excludes.
    #[error("{0}")]
    SecretAccessDenied(#[source] piqueld_store::StoreError),
    /// A build or fetch was requested for an environment that is now promoted.
    #[error("{0}")]
    EnvironmentPromoted(#[source] piqueld_store::StoreError),
    /// Repository input could not be located or decoded.
    #[error("{}", if *.not_found { "manifest not found" } else { "repository manifest is invalid or its application name does not match" })]
    ManifestInput {
        /// Whether the input was absent rather than invalid.
        not_found: bool,
        /// Internal I/O, validation, or checkout diagnostic.
        #[source]
        source: anyhow::Error,
    },
    /// A repository file a service mounts cannot be captured at the
    /// manifest's commit.
    #[error("{0}")]
    ManifestFile(#[from] piqueld_core::manifest::FileCaptureError),
    /// Operation execution was cancelled.
    #[error("operation was cancelled")]
    Cancelled,
    /// A newer application request made this operation obsolete.
    #[error("operation was superseded by a newer application request")]
    Superseded,
    /// A runtime resource is not safely owned by the application.
    #[error("a Docker resource is not safely owned by this application")]
    OwnershipConflict,
    /// An immutable Docker resource differs from the desired configuration.
    #[error("Docker resource configuration cannot be reconciled safely in place")]
    DockerConfigurationConflict,
    /// Docker is not an active Swarm manager.
    #[error("Docker is not an active Swarm manager")]
    SwarmManagerUnavailable,
    /// The Docker Swarm topology is unsupported.
    #[error("Docker Swarm must contain exactly one manager node")]
    SwarmTopologyUnsupported,
    /// Docker Engine is unavailable while performing the described operation.
    #[error("Docker Engine is unavailable while {0}")]
    DockerUnavailable(&'static str),
    /// An image could not be resolved while performing the described operation.
    #[error("container image could not be resolved to a digest while {0}")]
    ImageResolutionFailed(&'static str),
    /// The registry rejected the requested image or credentials.
    #[error("container image was rejected by the registry while {0}")]
    ImageResolutionRejected(&'static str),
    /// An image a deployment uses again is gone: a build, or a registry
    /// image that could not be pulled again by digest.
    #[error("service {}'s image {} is no longer present{}", .image.service, .image.image, if .source.is_some() { " and could not be pulled again" } else { ", and a build can't be pulled again" })]
    ImageUnavailable {
        /// The service and its missing image.
        image: piqueld_core::ServiceImage,
        /// Why pulling it again failed; absent for builds.
        #[source]
        source: Option<piqueld_docker::DockerError>,
    },
    /// Git source preparation failed before rollout.
    #[error("Git source build failed")]
    GitBuildFailed(#[source] anyhow::Error),
    /// The build machine could not be reached to build an image.
    #[error("the build machine is unreachable")]
    BuilderUnavailable,
    /// Image cleanup could not delete an image from the build registry.
    #[error("the build registry did not delete an image")]
    RegistryDeleteFailed,
    /// A Docker request failed while performing the described operation.
    #[error("Docker request failed while {0}")]
    DockerRequestFailed(&'static str),
    /// A local runtime value failed validation before a Docker request was made.
    #[error("Docker request validation failed while {0}")]
    ValidationFailed(&'static str),
    /// The configured repository manifest is missing.
    #[error("manifest not found")]
    ManifestNotFound,
    /// Repository access failed before reading its manifest.
    #[error("could not fetch the manifest repository")]
    ManifestFetchFailed(#[source] anyhow::Error),
    /// The fetched manifest is invalid or selects another application.
    #[error("repository manifest is invalid or its application name does not match")]
    ManifestInvalid,
    /// A one-shot job failed before rollout; prerequisites may have changed.
    #[error("{}", match .exit_code {
        Some(code) => format!("job {job} exited with status {code}; rollout stopped"),
        None => format!("job {job} stopped without an exit code; rollout stopped"),
    })]
    JobFailed {
        /// Logical job name.
        job: String,
        /// Container exit code, absent when Docker reported none.
        exit_code: Option<i64>,
    },
    /// A one-shot job exceeded its timeout and was stopped.
    #[error("job {0} did not finish before its timeout; rollout stopped")]
    JobTimeout(String),
    /// A service failed to roll out in Docker, as
    /// [`ObservedService::rollout_failure`](piqueld_core::ObservedService::rollout_failure)
    /// words it: which service, and Docker's reason when it gave one.
    #[error("{0}")]
    ServiceUpdateFailed(String),
    /// The runtime plan is blocked by a diagnostic without a specific mapping.
    #[error("runtime plan is blocked by {0}")]
    PlanBlocked(&'static str),
    /// A service, or the work around it, did not converge before its
    /// deadline.
    #[error("service did not converge before the deadline")]
    ConvergenceTimeout,
    /// A service the rollout waited for did not converge before its
    /// deadline, such as when its tasks stay pending. Its rollout failed, as
    /// with `ServiceUpdateFailed`.
    #[error("service did not converge before the deadline")]
    ServiceTimeout,
    /// Image resolution or the source build did not finish before its deadline.
    #[error("application preparation did not finish before the deadline")]
    PreparationTimeout,
}

impl OperationError {
    /// Returns the stable machine-readable failure code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.diagnostic_code().as_str()
    }

    /// Returns the failure an operation that failed with this error records.
    #[must_use]
    pub fn failure(&self) -> OperationFailure {
        OperationFailure {
            code: self.diagnostic_code(),
            message: self.message(),
        }
    }

    /// Returns the typed classification used by diagnostic policy.
    #[must_use]
    pub fn diagnostic_code(&self) -> DiagnosticCode {
        match self {
            Self::HostnameConflict(_) => DiagnosticCode::HostnameConflict,
            Self::Ingress(_) => DiagnosticCode::IngressUnavailable,
            Self::Docker(error) => error.diagnostic_code(),
            Self::Journal(_) => DiagnosticCode::JournalUnavailable,
            Self::SecretStorageUnavailable(_) => DiagnosticCode::SecretStorageUnavailable,
            Self::SecretUnavailable { .. } => DiagnosticCode::SecretUnavailable,
            Self::SecretMissing(_) => DiagnosticCode::SecretMissing,
            Self::SecretAccessDenied(_) => DiagnosticCode::SecretAccessDenied,
            Self::EnvironmentPromoted(_) => DiagnosticCode::EnvironmentPromoted,
            Self::ManifestInput {
                not_found: true, ..
            }
            | Self::ManifestNotFound => DiagnosticCode::ManifestNotFound,
            Self::ManifestInput {
                not_found: false, ..
            }
            | Self::ManifestFile(_)
            | Self::ManifestInvalid => DiagnosticCode::ManifestInvalid,
            Self::ImageUnavailable { .. } => DiagnosticCode::ImageUnavailable,
            Self::GitBuildFailed(_) => DiagnosticCode::GitBuildFailed,
            Self::BuilderUnavailable => DiagnosticCode::BuilderUnavailable,
            Self::RegistryDeleteFailed => DiagnosticCode::RegistryDeleteFailed,
            Self::Cancelled => DiagnosticCode::Cancelled,
            Self::Superseded => DiagnosticCode::Superseded,
            Self::OwnershipConflict => DiagnosticCode::OwnershipConflict,
            Self::DockerConfigurationConflict => DiagnosticCode::DockerConfigurationConflict,
            Self::SwarmManagerUnavailable => DiagnosticCode::SwarmManagerUnavailable,
            Self::SwarmTopologyUnsupported => DiagnosticCode::SwarmTopologyUnsupported,
            Self::DockerUnavailable(_) => DiagnosticCode::DockerUnavailable,
            Self::ImageResolutionFailed(_) => DiagnosticCode::ImageResolutionFailed,
            Self::ImageResolutionRejected(_) => DiagnosticCode::ImageResolutionRejected,
            Self::DockerRequestFailed(_) => DiagnosticCode::DockerRequestFailed,
            Self::ValidationFailed(_) => DiagnosticCode::ValidationFailed,
            Self::ManifestFetchFailed(_) => DiagnosticCode::ManifestFetchFailed,
            Self::ServiceUpdateFailed(_) => DiagnosticCode::ServiceUpdateFailed,
            Self::JobFailed { .. } => DiagnosticCode::JobFailed,
            Self::JobTimeout(_) => DiagnosticCode::JobTimeout,
            Self::PlanBlocked(_) => DiagnosticCode::PlanBlocked,
            Self::ConvergenceTimeout | Self::ServiceTimeout => DiagnosticCode::ConvergenceTimeout,
            Self::PreparationTimeout => DiagnosticCode::PreparationTimeout,
        }
    }

    /// What a deployment that failed with this error failed at (see
    /// [`DiagnosticCode::deployment_failure`]): a service that never
    /// converged during the rollout fails it like one Docker paused.
    #[must_use]
    pub fn deployment_failure(&self) -> Option<DeploymentFailure> {
        match self {
            Self::ServiceTimeout => Some(DeploymentFailure::Rollout),
            _ => self.diagnostic_code().deployment_failure(),
        }
    }

    /// Formats the stable, sanitized public failure message.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{self}")
    }

    /// A repository manifest that could not be read because of `source`: not
    /// found only when an I/O `NotFound` caused it, invalid otherwise.
    pub(crate) fn manifest_input(source: anyhow::Error) -> Self {
        let not_found = source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        Self::ManifestInput { not_found, source }
    }
}

impl From<piqueld_docker::DockerError> for OperationError {
    fn from(error: piqueld_docker::DockerError) -> Self {
        Self::Docker(error)
    }
}

impl OperationError {
    /// Projects a Docker failure onto its safe public classification without
    /// consuming the cause.
    fn docker_classification(error: &piqueld_docker::DockerError) -> Self {
        use piqueld_docker::DockerError;
        match error {
            DockerError::OwnershipConflict => Self::OwnershipConflict,
            DockerError::ConfigurationConflict => Self::DockerConfigurationConflict,
            DockerError::Validation(operation) => Self::ValidationFailed(operation),
            DockerError::NotManager => Self::SwarmManagerUnavailable,
            DockerError::IncompatibleSwarm => Self::SwarmTopologyUnsupported,
            DockerError::Unavailable(operation)
            | DockerError::UnavailableSource { operation, .. } => {
                Self::DockerUnavailable(operation)
            }
            DockerError::ImageResolutionSource { operation, .. } if error.registry_rejected() => {
                Self::ImageResolutionRejected(operation)
            }
            DockerError::ImageResolution(operation)
            | DockerError::ImageResolutionSource { operation, .. } => {
                Self::ImageResolutionFailed(operation)
            }
            DockerError::NetworkInUse(_) => {
                Self::DockerRequestFailed("delete a network a task still uses")
            }
            DockerError::NetworkRemoving(_) => {
                Self::DockerRequestFailed("delete a network Swarm is still removing")
            }
            DockerError::SecretInUse(_) => {
                Self::DockerRequestFailed("delete a secret a service still mounts")
            }
            DockerError::PortInUse(_) => {
                Self::DockerRequestFailed("start a container on a host port in use")
            }
            DockerError::BuilderUnavailable(_) => Self::BuilderUnavailable,
            DockerError::Registry(_) => Self::RegistryDeleteFailed,
            DockerError::Request(operation) | DockerError::RequestSource { operation, .. } => {
                Self::DockerRequestFailed(operation)
            }
        }
    }
}

impl From<piqueld_store::StoreError> for OperationError {
    /// Keeps secret and hostname failures distinct; every other storage error
    /// is a journal failure.
    fn from(error: piqueld_store::StoreError) -> Self {
        match error {
            error @ piqueld_store::StoreError::SecretSource(_) => {
                Self::SecretStorageUnavailable(error)
            }
            piqueld_store::StoreError::SecretUnavailable { names } => {
                Self::SecretUnavailable { names }
            }
            error @ piqueld_store::StoreError::SecretMissing { .. } => Self::SecretMissing(error),
            error @ piqueld_store::StoreError::SecretAccessDenied { .. } => {
                Self::SecretAccessDenied(error)
            }
            error @ piqueld_store::StoreError::Promotion(
                piqueld_store::PromotionError::Promoted { .. },
            ) => Self::EnvironmentPromoted(error),
            error @ (piqueld_store::StoreError::HostnameConflict { .. }
            | piqueld_store::StoreError::SharedHostnameConflict { .. }) => {
                Self::HostnameConflict(error)
            }
            other => Self::Journal(other),
        }
    }
}

impl OperationError {
    /// Creates a public diagnostic using only explicitly safe source fields.
    #[must_use]
    pub fn diagnostic(&self) -> piqueld_core::observability::Diagnostic {
        let source: &(dyn std::error::Error + 'static) = match self {
            Self::GitBuildFailed(error) | Self::ManifestFetchFailed(error) => error.as_ref(),
            _ => self,
        };
        Self::diagnostic_from(self.diagnostic_code(), self.message(), source)
    }

    /// Diagnoses a failed ingress action without taking ownership of its error.
    /// `code` is `IngressUnavailable` for gateway changes and
    /// `CertificateRenewalFailed` for DNS-01 certificates and
    /// `DnsRecordsFailed` for route DNS records.
    pub(crate) fn ingress_diagnostic(
        code: DiagnosticCode,
        error: &anyhow::Error,
    ) -> piqueld_core::observability::Diagnostic {
        let summary = match code {
            DiagnosticCode::CertificateRenewalFailed => {
                "a DNS-01 certificate could not be issued or renewed; see certificate status and daemon logs"
            }
            DiagnosticCode::DnsRecordsFailed => {
                "a route's DNS records could not be updated; see route status and daemon logs"
            }
            _ => {
                "managed ingress could not apply a gateway change; see ingress health and daemon logs"
            }
        };
        Self::diagnostic_from(code, summary.into(), error.as_ref())
    }

    /// Builds a diagnostic with a fresh ID, then walks up to eight levels of the
    /// source chain, adding only safe typed facts (Docker operation names, HTTP
    /// statuses, I/O kinds, OS and database codes, command stages) as causes.
    /// Error messages themselves are never copied, since they may contain secrets.
    fn diagnostic_from(
        code: DiagnosticCode,
        summary: String,
        source: &(dyn std::error::Error + 'static),
    ) -> piqueld_core::observability::Diagnostic {
        let mut diagnostic =
            piqueld_core::observability::Diagnostic::new(diagnostic_id(), code, summary);
        let mut source = Some(source);
        for _ in 0..8 {
            let Some(error) = source else {
                break;
            };
            if let Some(error) = error.downcast_ref::<piqueld_docker::DockerError>() {
                use piqueld_docker::DockerError;
                let operation = match error {
                    DockerError::Unavailable(operation)
                    | DockerError::ImageResolution(operation)
                    | DockerError::Request(operation)
                    | DockerError::Validation(operation)
                    | DockerError::UnavailableSource { operation, .. }
                    | DockerError::ImageResolutionSource { operation, .. }
                    | DockerError::RequestSource { operation, .. } => Some(*operation),
                    DockerError::NetworkInUse(_) | DockerError::NetworkRemoving(_) => {
                        Some("delete network")
                    }
                    DockerError::PortInUse(_) => Some("start container"),
                    _ => None,
                };
                if let Some(operation) = operation {
                    diagnostic
                        .causes
                        .push(format!("Docker operation: {operation}"));
                }
            }
            if let Some(failure) = error
                .downcast_ref::<piqueld_store::secrets::SecretError>()
                .and_then(piqueld_store::secrets::SecretError::key_failure)
            {
                diagnostic.causes.push(failure.fact().into());
            }
            if let Some(error) = error.downcast_ref::<piqueld_docker::ErrorResponse>() {
                diagnostic.causes.push(format!(
                    "Docker returned HTTP status {}",
                    error.status().as_u16()
                ));
            }
            if let Some(error) = error.downcast_ref().and_then(UnixHttpError::response) {
                diagnostic.causes.push(format!(
                    "Ingress API returned HTTP status {}",
                    error.status.as_u16()
                ));
            }
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                diagnostic
                    .causes
                    .push(format!("I/O failure: {:?}", error.kind()));
                if let Some(code) = error.raw_os_error() {
                    diagnostic
                        .causes
                        .push(format!("Operating system error code: {code}"));
                }
            }
            if let Some(bollard::errors::Error::DockerResponseServerError { status_code, .. }) =
                error.downcast_ref::<bollard::errors::Error>()
            {
                diagnostic
                    .causes
                    .push(format!("Docker returned HTTP status {status_code}"));
            }
            if let Some(error) = error.downcast_ref().and_then(CommandError::failure) {
                diagnostic
                    .causes
                    .push(format!("Command stage: {}", error.operation));
                diagnostic.causes.push(error.status.code().map_or_else(
                    || "Command terminated without an exit code".into(),
                    |code| format!("Command exit code: {code}"),
                ));
            }
            if let Some(error) = error.downcast_ref::<sqlx::Error>() {
                match error {
                    sqlx::Error::Database(database) => {
                        diagnostic
                            .causes
                            .push(format!("Database failure: {:?}", database.kind()));
                        if let Some(code) =
                            database.code().and_then(|code| code.parse::<i64>().ok())
                        {
                            diagnostic
                                .causes
                                .push(format!("Database error code: {code}"));
                        }
                    }
                    sqlx::Error::PoolTimedOut => diagnostic
                        .causes
                        .push("Timed out waiting for a database connection".into()),
                    sqlx::Error::PoolClosed => diagnostic
                        .causes
                        .push("Database connection pool is closed".into()),
                    _ => {}
                }
            }
            source = error.source();
        }
        diagnostic
    }
}

impl crate::application::BoundaryError {
    /// Diagnoses a runtime-boundary failure (API errors, journaled source
    /// preparation, API-driven secret removal) using the same safe-cause rules.
    /// Compilation failures list up to 16 resource errors as causes.
    pub(crate) fn diagnostic(&self) -> piqueld_core::observability::Diagnostic {
        let (code, summary) = match self {
            Self::Runtime(error) => {
                let classification = OperationError::docker_classification(error);
                (classification.diagnostic_code(), classification.message())
            }
            Self::Store(_) => (
                DiagnosticCode::JournalUnavailable,
                "Control-plane storage is unavailable".into(),
            ),
            Self::GitBuild(_) => (
                DiagnosticCode::GitBuildFailed,
                "Git source build failed; inspect the associated build output".into(),
            ),
            Self::Compilation(_) => (
                DiagnosticCode::ApplicationCompilationFailed,
                "Resolved application could not be compiled".into(),
            ),
            Self::ImageUnavailable(error) => (error.diagnostic_code(), error.message()),
        };
        let source: &(dyn std::error::Error + 'static) = match self {
            Self::GitBuild(error) => error.as_ref(),
            _ => self,
        };
        let mut diagnostic = OperationError::diagnostic_from(code, summary, source);
        if let Self::Compilation(errors) = self {
            diagnostic.causes.extend(
                errors
                    .iter()
                    .take(16)
                    .map(|error| format!("{}: {} ({})", error.resource, error.message, error.code)),
            );
        }
        diagnostic
    }
}

#[cfg(test)]
mod tests;
