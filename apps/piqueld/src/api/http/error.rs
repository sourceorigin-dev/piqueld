//! The API's structured error: every failure a handler or middleware
//! reports becomes an [`ApiError`], rendered as a JSON `ErrorBody` with a
//! stable `code`.
use super::JSON;
use crate::api::ApplicationError;
use crate::application::BoundaryError;
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{ApiErrorCode, ErrorBody};
use piqueld_core::{
    ApplicationIdError, EnvironmentIdError, EnvironmentNameError, GitBranchError, PreviewSlotError,
};
use piqueld_store::{PromotionError, RestoreError, StoreError};
use serde_json::{Value, json};

/// Handler failure rendered as a structured JSON `ErrorBody` response.
///
/// Every domain error converts into this type, so handlers can use `?` and
/// still produce a stable machine-readable `code` plus a sanitized message.
#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: StatusCode,
    /// Stable machine-readable error code.
    code: ApiErrorCode,
    /// Sanitized, user-facing explanation.
    message: String,
    /// Extra structured context; `null` when there is none.
    details: Value,
    /// `Allow` header value, only set for 405 responses.
    pub(super) allow: Option<String>,
    /// Diagnostic attached as a response extension so `bind_error_request_id`
    /// can record it instead of synthesizing a generic one.
    pub(super) diagnostic: Option<Box<piqueld_core::observability::Diagnostic>>,
    /// Authorization refusal attached as a response extension, so the audit
    /// trail records it even when it reads as a plain 404.
    pub(super) denied: Option<piqueld_core::access::Denied>,
}

impl ApiError {
    /// Creates an error without details, `Allow` header, or diagnostic.
    pub(super) fn new(status: StatusCode, code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Value::Null,
            allow: None,
            diagnostic: None,
            denied: None,
        }
    }
    /// The 404 for an API path no route matches.
    pub(super) fn endpoint_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ApiErrorCode::EndpointNotFound,
            "API endpoint was not found",
        )
    }

    /// The 404 for a resource that doesn't exist, or the caller may not see.
    pub(super) fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ApiErrorCode::NotFound,
            "resource was not found",
        )
    }

    /// Attaches structured `details` to the error body.
    pub(super) fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Logs storage failures that indicate a server-side fault. Expected client
    /// errors (conflicts, not found, validation) are deliberately not logged.
    fn log_storage_error(error: &StoreError) {
        if matches!(
            error,
            StoreError::SecretSource(_)
                | StoreError::Database
                | StoreError::DatabaseSource(_)
                | StoreError::SchemaMismatch
                | StoreError::SchemaMismatchSource(_)
                | StoreError::PathSource(_)
                | StoreError::Corrupt
                | StoreError::CorruptSource(_)
        ) {
            tracing::error!(?error, "storage request failed");
        }
    }
}

impl ApiError {
    /// Maps secret lifecycle failures selected by `From<StoreError>`.
    ///
    /// Panics if given a non-secret variant; callers must pre-filter.
    fn from_secret_error(error: StoreError) -> Self {
        match error {
            StoreError::SecretVersionConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretGenerationConflict,
                "Secret changed since inspection; read its metadata and retry",
            )
            .details(json!({"expected_generation": expected, "actual_generation": actual})),
            StoreError::SecretUnavailable { names } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretUnavailable,
                "Supply replacement secret values and start a new deployment",
            )
            .details(json!({"names": names})),
            ref error @ StoreError::SecretMissing { ref names } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretMissing,
                error.to_string(),
            )
            .details(json!({"names": names})),
            StoreError::SecretKeyUsable => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretKeyUsable,
                "The secret master key still works; recovery would discard values needlessly",
            ),
            error @ StoreError::SecretSource(_) => {
                let diagnostic = crate::operations::OperationError::from(error).diagnostic();
                let mut error = Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ApiErrorCode::SecretStorageUnavailable,
                    "Secret storage is unavailable",
                );
                error.diagnostic = Some(Box::new(diagnostic));
                error
            }
            StoreError::SecretDeleting => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretDeleting,
                "Secret deletion is in progress; retry deletion to finish cleanup",
            ),
            StoreError::SecretAccessConflict => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretAccessConflict,
                "The secret's access changed since you read it; read it again and retry",
            ),
            StoreError::SecretQuota => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretQuotaExceeded,
                "Secret storage quota exceeded (100 secrets, 1000 versions or 100 MiB per environment or application store); delete unused secrets to free space",
            ),
            StoreError::SecretReferenced => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretReferenced,
                "Secret is still referenced by application configuration or a deployment",
            ),
            ref error @ StoreError::SecretAccessDenied {
                ref environment,
                ref secret,
            } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SecretAccessDenied,
                error.to_string(),
            )
            .details(json!({"environment": environment, "secret": secret})),
            #[expect(
                clippy::unreachable,
                reason = "`From<StoreError>` sends this function only the variants matched above"
            )]
            other => unreachable!("non-secret storage error: {other}"),
        }
    }

    /// Maps environment selection and rename failures selected by
    /// `From<StoreError>`, naming the environments in `details`, previews
    /// of applications without a manifest repository, and previews over a
    /// `[previews]` limit, listing the previews it counts in `details`.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_environment_error(error: StoreError) -> Self {
        match error {
            StoreError::EnvironmentRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentRequired,
                "The application does not have exactly one environment; name the environment explicitly",
            )
            .details(json!({"environments": environments})),
            StoreError::ConfirmationRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentConfirmationRequired,
                "Deleting this application also deletes all its environments; confirm by naming every one",
            )
            .details(json!({"environments": environments})),
            ref error @ StoreError::EnvironmentConfigured { ref environment } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentConfigured,
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::EnvironmentDeclared { ref environment } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentDeclared,
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::EnvironmentUnknown { ref environment } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::EnvironmentUnknown,
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::PreviewRequiresRepository => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::PreviewRequiresRepository,
                error.to_string(),
            ),
            ref error @ StoreError::PreviewLimitReached(ref reached) => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::PreviewLimitReached,
                error.to_string(),
            )
            .details(json!(reached)),
            #[expect(
                clippy::unreachable,
                reason = "`From<StoreError>` sends this function only the variants matched above"
            )]
            other => unreachable!("non-environment storage error: {other}"),
        }
    }

    /// Maps refused promotions and promotion sources, with what they name in
    /// `details`. Changes the source made since it was inspected are
    /// conflicts; impossible sources are invalid input.
    fn from_promotion_error(error: &PromotionError) -> Self {
        let (status, code, details) = match error {
            PromotionError::Promoted { environment } => (
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentPromoted,
                json!({"environment": environment}),
            ),
            PromotionError::NotPromoted { environment } => (
                StatusCode::CONFLICT,
                ApiErrorCode::EnvironmentNotPromoted,
                json!({"environment": environment}),
            ),
            PromotionError::Cycle { environments } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::PromotionCycle,
                json!({"environments": environments}),
            ),
            PromotionError::SourceInvalid { environment, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::PromotionSourceInvalid,
                json!({"environment": environment}),
            ),
            PromotionError::InUse { environments } => (
                StatusCode::CONFLICT,
                ApiErrorCode::PromotionSourceInUse,
                json!({"environments": environments}),
            ),
            PromotionError::SourceNotReady { environment, .. } => (
                StatusCode::CONFLICT,
                ApiErrorCode::PromotionSourceNotReady,
                json!({"environment": environment}),
            ),
            PromotionError::SourceChanged {
                environment,
                deployment,
            } => (
                StatusCode::CONFLICT,
                ApiErrorCode::PromotionSourceChanged,
                json!({"environment": environment, "deployment": deployment}),
            ),
            PromotionError::Secrets {
                environment,
                secrets,
            } => (
                StatusCode::CONFLICT,
                ApiErrorCode::SecretsUnavailable,
                json!({"environment": environment, "secrets": secrets}),
            ),
        };
        Self::new(status, code, error.to_string()).details(details)
    }

    /// Maps refused restores, with the deployment and what it lacks in
    /// `details`. Both are conflicts: the deployment exists, but can't run
    /// again as it did.
    fn from_restore_error(error: &RestoreError) -> Self {
        let (code, details) = match error {
            RestoreError::NotRestorable { deployment, .. } => (
                ApiErrorCode::DeploymentNotRestorable,
                json!({"deployment": deployment}),
            ),
            RestoreError::SecretsGone {
                deployment,
                secrets,
            } => (
                ApiErrorCode::RestoreSecretsUnavailable,
                json!({"deployment": deployment, "secrets": secrets}),
            ),
        };
        Self::new(StatusCode::CONFLICT, code, error.to_string()).details(details)
    }

    /// Maps hostname reservation conflicts selected by `From<StoreError>`,
    /// naming the hostname, and the sibling environment reserving it if any.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_hostname_conflict(error: StoreError) -> Self {
        match error {
            StoreError::HostnameConflict { hostname } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::HostnameConflict,
                "Hostname is reserved by another environment, another application's wildcard, or this installation",
            )
            .details(json!({"hostname": hostname})),
            ref error @ StoreError::SharedHostnameConflict {
                ref hostname,
                ref environment,
            } => Self::new(StatusCode::CONFLICT, ApiErrorCode::HostnameConflict, error.to_string())
                .details(json!({"hostname": hostname, "environment": environment})),
            #[expect(
                clippy::unreachable,
                reason = "`From<StoreError>` sends this function only the variants matched above"
            )]
            other => unreachable!("non-hostname storage error: {other}"),
        }
    }
}

impl From<StoreError> for ApiError {
    /// Classifies storage failures into HTTP status codes and stable error
    /// codes, logging server-side faults first.
    fn from(value: StoreError) -> Self {
        Self::log_storage_error(&value);
        match value {
            StoreError::Validation(errors) => errors.into(),
            StoreError::Edit(error) => error.into(),
            error @ (StoreError::SecretVersionConflict { .. }
            | StoreError::SecretUnavailable { .. }
            | StoreError::SecretMissing { .. }
            | StoreError::SecretKeyUsable
            | StoreError::SecretSource(_)
            | StoreError::SecretDeleting
            | StoreError::SecretAccessConflict
            | StoreError::SecretQuota
            | StoreError::SecretReferenced
            | StoreError::SecretAccessDenied { .. }) => Self::from_secret_error(error),
            error @ (StoreError::HostnameConflict { .. }
            | StoreError::SharedHostnameConflict { .. }) => Self::from_hostname_conflict(error),
            error @ (StoreError::EnvironmentRequired { .. }
            | StoreError::ConfirmationRequired { .. }
            | StoreError::EnvironmentConfigured { .. }
            | StoreError::EnvironmentDeclared { .. }
            | StoreError::EnvironmentUnknown { .. }
            | StoreError::PreviewRequiresRepository
            | StoreError::PreviewLimitReached(_)) => Self::from_environment_error(error),
            StoreError::Promotion(error) => Self::from_promotion_error(&error),
            StoreError::Restore(error) => Self::from_restore_error(&error),
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::GenerationConflict,
                "Application changed since inspection; run the command again",
            )
            .details(json!({"expected_generation": expected, "actual_generation": actual})),
            StoreError::IdentityConflict => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::IdentityConflict,
                "Application changed since inspection; run the command again",
            ),
            StoreError::ReplayConflict => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::RequestIdConflict,
                "request ID was already used for different input",
            ),
            StoreError::RepositoryManaged => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::RepositoryManaged,
                "Rename a repository-backed application while it is disconnected, then rename it in its repository manifest",
            ),
            StoreError::Busy => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::ApplicationBusy,
                "application is busy; wait for its current operation to finish",
            ),
            StoreError::HistoryExpired => Self::new(
                StatusCode::GONE,
                ApiErrorCode::HistoryExpired,
                "Requested event history was pruned; reload history before resuming",
            ),
            StoreError::NotFound => Self::not_found(),
            StoreError::Lockout(lockout) => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::AccountLockout,
                lockout.message(),
            ),
            StoreError::Destination(error) => error.into(),
            StoreError::Retry(error) => error.into(),
            StoreError::Denied(denied) => denied.into(),
            StoreError::CredentialRevoked => crate::auth::AuthError::Unauthorized.into(),
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::ApplicationNameCollision,
                "application, environment, or preview identity or name already exists",
            ),
            StoreError::IllegalTransition => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::ApplicationStateConflict,
                "the requested application transition is not allowed",
            ),
            StoreError::InvalidInput | StoreError::InvalidInputSource(_) => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::InvalidRequest,
                "the request is invalid",
            ),
            StoreError::SchemaMismatch | StoreError::SchemaMismatchSource(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCode::SchemaMismatch,
                "database schema is incompatible",
            ),
            StoreError::Corrupt | StoreError::CorruptSource(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiErrorCode::StoredStateCorrupt,
                "stored application state is corrupt",
            ),
            StoreError::Database | StoreError::DatabaseSource(_) | StoreError::PathSource(_) => {
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ApiErrorCode::StorageUnavailable,
                    "control-plane storage is unavailable",
                )
            }
        }
    }
}

impl From<piqueld_store::DestinationError> for ApiError {
    /// Rejects an invalid destination change with 400.
    fn from(error: piqueld_store::DestinationError) -> Self {
        use piqueld_store::DestinationError;
        let code = match error {
            DestinationError::UrlRequired => ApiErrorCode::DestinationUrlRequired,
            DestinationError::UrlInvalid => ApiErrorCode::DestinationUrlInvalid,
            DestinationError::UnknownApplication(_) => ApiErrorCode::DestinationApplicationUnknown,
        };
        Self::new(StatusCode::BAD_REQUEST, code, error.to_string())
    }
}

impl From<piqueld_store::RetryError> for ApiError {
    /// Refuses a delivery that cannot be retried with 409, saying why.
    fn from(error: piqueld_store::RetryError) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            ApiErrorCode::DeliveryNotRetryable,
            error.to_string(),
        )
    }
}

impl From<piqueld_core::edit::EditError> for ApiError {
    /// Maps field-edit failures, exposing the edit reason in `details.reason`.
    fn from(error: piqueld_core::edit::EditError) -> Self {
        use piqueld_core::edit::EditError;
        let (status, code) = match &error {
            EditError::NotFound { .. } => {
                (StatusCode::NOT_FOUND, ApiErrorCode::FieldResourceNotFound)
            }
            EditError::AlreadyExists { .. } => {
                (StatusCode::CONFLICT, ApiErrorCode::FieldResourceExists)
            }
            EditError::Incompatible(_) => (StatusCode::CONFLICT, ApiErrorCode::FieldIncompatible),
        };
        Self::new(status, code, "The field edit could not be applied")
            .details(json!({"reason": error.to_string()}))
    }
}

impl From<BoundaryError> for ApiError {
    /// Maps runtime, build, and compilation failures, always attaching the
    /// boundary diagnostic so it is recorded with the request.
    fn from(value: BoundaryError) -> Self {
        let diagnostic = value.diagnostic();
        if !matches!(&value, BoundaryError::Store(_)) {
            tracing::error!(error = ?value, "runtime boundary request failed");
        }
        let mut error = match value {
            BoundaryError::Store(error) => error.into(),
            BoundaryError::Runtime(
                piqueld_docker::DockerError::Unavailable(_)
                | piqueld_docker::DockerError::UnavailableSource { .. },
            ) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCode::DockerUnavailable,
                "Docker Engine is unavailable",
            ),
            BoundaryError::Runtime(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                ApiErrorCode::RuntimeRequestFailed,
                "runtime request failed",
            ),
            BoundaryError::GitBuild(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                ApiErrorCode::GitBuildFailed,
                "Git source build failed",
            ),
            BoundaryError::Compilation(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiErrorCode::ApplicationCompilationFailed,
                "application compilation failed",
            ),
            BoundaryError::ImageUnavailable(error) => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::ImageUnavailable,
                error.to_string(),
            ),
        };
        error.diagnostic = Some(Box::new(diagnostic));
        error
    }
}

/// Implements `From` for errors rejecting a malformed identifier or name in
/// a request, answering 400 with `$code` and the message `$message` builds
/// from the error.
macro_rules! invalid_input {
    ($($error:ty => $code:path, |$value:pat_param| $message:expr;)+) => {$(
        impl From<$error> for ApiError {
            fn from(error: $error) -> Self {
                let $value = error;
                Self::new(StatusCode::BAD_REQUEST, $code, $message)
            }
        }
    )+};
}

invalid_input! {
    ApplicationIdError => ApiErrorCode::ApplicationIdInvalid, |_| "application ID is invalid";
    EnvironmentIdError => ApiErrorCode::EnvironmentIdInvalid, |_| "environment ID is invalid";
    EnvironmentNameError => ApiErrorCode::EnvironmentNameInvalid, |_| "environment names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit";
    GitBranchError => ApiErrorCode::GitBranchInvalid, |error| error.to_string();
    PreviewSlotError => ApiErrorCode::PreviewSlotInvalid, |error| error.to_string();
    piqueld_core::observability::DestinationNameError => ApiErrorCode::DestinationNameInvalid, |error| error.to_string();
}

impl From<piqueld_core::ValidationErrors> for ApiError {
    /// Reports pure decode failures as `toml_malformed` (400) and semantic
    /// validation failures as `manifest_validation_failed` (422). Both list
    /// the individual errors in `details.errors`.
    fn from(piqueld_core::ValidationErrors(errors): piqueld_core::ValidationErrors) -> Self {
        let error = if errors
            .iter()
            .all(|error| error.code == ApiErrorCode::ManifestDecodeFailed)
        {
            Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::TomlMalformed,
                "request TOML is malformed or does not match the application schema",
            )
        } else {
            Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::ManifestValidationFailed,
                "application manifest failed validation",
            )
        };
        error.details(json!({"errors": errors}))
    }
}

impl ApiError {
    /// Builds the public JSON error body. Its placeholder `request_id` is
    /// replaced with the real one by `bind_error_request_id`, or by the exec
    /// stream before it writes a terminal failure frame.
    pub(super) fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code.into(),
            message: self.message.clone(),
            details: self.details.clone(),
            request_id: uuid::Uuid::now_v7().simple().to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    /// Renders the JSON error body with the error's status, diagnostic
    /// extension, and optional `Allow` header.
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(self.body()),
        )
            .into_response();
        if let Some(diagnostic) = self.diagnostic {
            response.extensions_mut().insert(*diagnostic);
        }
        if let Some(denied) = self.denied {
            response.extensions_mut().insert(denied);
        }
        if let Some(allow) = self.allow
            && let Ok(value) = header::HeaderValue::from_str(&allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

impl From<ApplicationError> for ApiError {
    /// Maps application service failures, delegating storage and runtime
    /// errors to their dedicated conversions.
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::PreconditionRequired => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::PreconditionRequired,
                "Supply the inspected revision and application identity, or explicitly set force=true",
            ),
            ApplicationError::InvalidPagination => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::PaginationInvalid,
                "pagination parameters are invalid",
            ),
            ApplicationError::InvalidLogQuery => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::LogsQueryInvalid,
                "Tail must be 1–1000 and time window 1–86400 seconds",
            ),
            ApplicationError::InvalidTrafficQuery => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::TrafficQueryInvalid,
                "The window must start at or after Unix ms 0, end after it starts, and span at most 400 days",
            ),
            ApplicationError::ServiceNotRunning => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::ServiceNotRunning,
                "Service has no running task; deploy it or check its health",
            ),
            ApplicationError::ConfigurationUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCode::ConfigurationUnavailable,
                "Effective host configuration is unavailable",
            ),
            ApplicationError::ManifestSerialization(error) => {
                tracing::error!(?error, "serialize saved manifest");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorCode::ManifestSerializationFailed,
                    "Could not render saved configuration",
                )
            }
            ApplicationError::Store(error) => error.into(),
            ApplicationError::Runtime(error) => error.into(),
            ApplicationError::PlanBlocked(diagnostics) => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::PlanBlocked,
                "runtime plan contains blocking conflicts",
            )
            .details(json!({"diagnostics":diagnostics})),
            ApplicationError::RepositoryUnavailable(error) => {
                tracing::warn!(?error, "manifest repository unavailable");
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    ApiErrorCode::RepositoryUnavailable,
                    format!(
                        "The manifest repository could not be read ({error:#}); no preview was deleted"
                    ),
                )
            }
            ApplicationError::Snapshot(error) => error.into(),
            ApplicationError::Busy(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCode::ServerBusy,
                "Too many requests like this one are running; try again shortly",
            ),
        }
    }
}

impl From<crate::git::snapshot::SnapshotError> for ApiError {
    /// Maps refused snapshot uploads; Git's output stays in the logs.
    fn from(error: crate::git::snapshot::SnapshotError) -> Self {
        use crate::git::snapshot::SnapshotError;
        let message = error.to_string();
        match error {
            SnapshotError::TooLarge { limit } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ApiErrorCode::SnapshotTooLarge,
                format!("{message}; raise [snapshots] max_upload_bytes, or upload against a base the repository has"),
            )
            .details(json!({ "limit": limit })),
            SnapshotError::TreeTooLarge { limit } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ApiErrorCode::SnapshotTooLarge,
                format!("{message}; raise [checkouts] max_bytes, or ignore the files the preview doesn't need"),
            )
            .details(json!({ "limit": limit })),
            SnapshotError::Invalid(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::SnapshotInvalid,
                message,
            ),
            SnapshotError::BaseUnavailable { prerequisites } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::SnapshotBaseUnavailable,
                format!("{message}; push the base, or upload a complete bundle"),
            )
            .details(json!({ "prerequisites": prerequisites })),
            SnapshotError::TimedOut => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                ApiErrorCode::SnapshotTimedOut,
                message,
            ),
            SnapshotError::Internal(error) => {
                tracing::error!(?error, "snapshot upload failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorCode::SnapshotUnavailable,
                    message,
                )
            }
        }
    }
}
