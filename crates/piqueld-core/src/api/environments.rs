//! Environments: their views, requests, status and observed runtime.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{ApplicationView, RouteUrl, TelemetryReport};
use crate::manifest::{ApplicationTemplate, RenderTarget};
use crate::{
    ApplicationId, ApplicationState, Convergence, EnvironmentId, EnvironmentKind, EnvironmentName,
    EnvironmentSource, Operation, PlanDiagnostic, Preview, ReleaseId,
};

/// A deployable unit of an application, with its own deployment history,
/// status, volumes, generated secrets, routes, and Docker network.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct EnvironmentView {
    /// Stable environment identifier; runtime names derive from it.
    pub id: EnvironmentId,
    /// Owning application.
    pub application_id: ApplicationId,
    /// Name, unique within the application; a preview's slug.
    pub name: EnvironmentName,
    /// Where deployments come from; a preview's branch.
    pub source: EnvironmentSource,
    /// An environment, or a preview of a branch.
    #[serde(default)]
    pub kind: EnvironmentKind,
    /// Whether this environment opted into its application's sync (see
    /// [`Self::sync_state`]). Previews follow their application's setting.
    #[serde(default)]
    pub sync: bool,
    /// The branch head as of its last deployment, by sync or of its own
    /// branch; absent before its first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<crate::sync::SyncedHead>,
    /// Whether it is granted the daemon's telemetry, which its receiving
    /// service, if it has one, then gets from every application. Previews
    /// never are.
    #[serde(default)]
    pub telemetry: bool,
    /// Whether it runs its services, or is stopped until it is started or
    /// deployed again.
    #[serde(default)]
    pub run_state: RunState,
    /// Application revision of the last completely resolved target, not a
    /// convergence guarantee.
    pub resolved_generation: Option<u64>,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// An environment as the application listing shows it: with its status and
/// latest operation, so a listing needs no request per environment.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct EnvironmentSummary {
    /// The environment.
    #[serde(flatten)]
    pub environment: EnvironmentView,
    /// Its last recorded status; absent before one is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EnvironmentStatusView>,
    /// Its newest operation, a deployment unless it is being deleted; absent
    /// before its first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_operation: Option<Operation>,
}

crate::names::string_enum! {
    #[serde]
    /// Whether an environment runs its services.
    #[derive(Default)]
    pub enum RunState {
        /// Its services run their replicas and the gateway serves its routes.
        #[default]
        Running => "running",
        /// Its services run no replicas, keeping their configuration and
        /// volumes, and the gateway serves none of its routes.
        Stopped => "stopped",
    }
}

impl RunState {
    /// `Stopped` when `stopped`, else `Running`.
    #[must_use]
    pub const fn new(stopped: bool) -> Self {
        if stopped {
            Self::Stopped
        } else {
            Self::Running
        }
    }
}

impl EnvironmentView {
    /// Whether it is stopped (see [`RunState::Stopped`]).
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.run_state == RunState::Stopped
    }

    /// What its manifest renders for: its own block, or `[spec.previews]`.
    #[must_use]
    pub fn target(&self) -> RenderTarget {
        match &self.kind {
            EnvironmentKind::Environment => RenderTarget::Environment(self.name.clone()),
            EnvironmentKind::Preview(preview) => RenderTarget::Preview(preview.slug.clone()),
        }
    }

    /// The preview's identity, unless this is an environment.
    #[must_use]
    pub const fn preview(&self) -> Option<&Preview> {
        match &self.kind {
            EnvironmentKind::Environment => None,
            EnvironmentKind::Preview(preview) => Some(preview),
        }
    }
}

/// Renames an environment, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRequest {
    /// Environment name, unique within the application.
    pub name: String,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Creates an environment, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEnvironmentRequest {
    /// Environment name, unique within the application.
    pub name: String,
    /// Branch of the application's manifest repository to follow. Defaults to
    /// the branch `spec.manifest` names; only for repository-backed applications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full commit to pin instead of following `branch`'s head; requires `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// ID of an environment to receive releases from instead of building;
    /// excludes `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promote_from: Option<String>,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Makes an environment promoted from another, or returns a promoted one
/// to tracking its application's saved manifest or the branch
/// `spec.manifest` names, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSourceRequest {
    /// ID of the environment to receive releases from; absent to track again.
    #[serde(default)]
    pub promote_from: Option<String>,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Points an environment of a repository-backed application at another
/// branch, or pins or unpins its commit, conditioned on the inspected
/// application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentBranchRequest {
    /// Branch of the application's manifest repository to follow.
    pub branch: String,
    /// Full commit to pin instead of following the branch head.
    #[serde(default)]
    pub commit: Option<String>,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Current environment reconciliation status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct EnvironmentStatusView {
    /// Stable environment identifier.
    pub environment_id: String,
    /// Machine-readable lifecycle state.
    pub state: ApplicationState,
    /// Observed runtime health, independent of operation progress.
    pub runtime_health: Option<String>,
    /// When `runtime_health` was last observed, in Unix milliseconds; it is
    /// refreshed on every health scan, so an old value means it is stale.
    #[serde(default)]
    pub health_observed_at_ms: Option<i64>,
    /// Optional safe status message.
    pub message: Option<String>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// Sanitized runtime diagnostic shown by the read-only dashboard.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DiagnosticView {
    /// Stable diagnostic category.
    pub code: String,
    /// Bounded, actionable message safe for a browser.
    pub message: String,
}

impl DiagnosticView {
    /// A diagnostic with the stable `code`.
    #[must_use]
    pub fn new(code: crate::ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Keeps a planner warning's code and message, e.g. to record it on a
/// deployment.
impl From<PlanDiagnostic> for DiagnosticView {
    fn from(diagnostic: PlanDiagnostic) -> Self {
        Self {
            code: diagnostic.code,
            message: diagnostic.message,
        }
    }
}

/// Observed service state summarized for browser and operator clients.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ObservedServiceView {
    /// Logical service name from the desired application.
    pub name: String,
    /// Observed immutable image reference, when the service exists.
    pub image: Option<String>,
    /// Desired replica count from the application manifest.
    pub desired_replicas: u16,
    /// Replicas currently reported by the runtime.
    pub observed_replicas: u16,
    /// Replicas that are running and healthy enough to serve traffic.
    pub healthy_replicas: u16,
    /// Runtime convergence category.
    pub convergence: Convergence,
    /// Sanitized task and service diagnostics.
    pub diagnostics: Vec<DiagnosticView>,
}

/// Bounded observed runtime state for one environment.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ObservedApplicationView {
    /// Observed services in desired service order.
    pub services: Vec<ObservedServiceView>,
    /// Number of owned networks observed.
    pub network_count: u32,
    /// Number of owned volumes observed.
    pub volume_count: u32,
}

/// Read-only environment detail composed at the API boundary.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct EnvironmentDetailView {
    /// The environment.
    pub environment: EnvironmentView,
    /// Its application and shared desired configuration.
    pub application: ApplicationView,
    /// The manifest this environment deploys, with references unresolved: the
    /// last one fetched from its branch, the application's saved manifest, or
    /// that of the release last promoted into it. Before a promoted
    /// environment's first promotion, that of the release its source runs,
    /// which promoting would deploy. Absent until a repository-backed
    /// environment's first fetch.
    pub manifest: Option<ApplicationTemplate>,
    /// Durable environment lifecycle status.
    pub status: EnvironmentStatusView,
    /// Sanitized runtime observation.
    pub observed: ObservedApplicationView,
    /// Most recent durable operation, when one exists.
    pub latest_operation: Option<Operation>,
    /// The release the current runtime target runs; absent before a
    /// deployment was prepared, and for deployments that recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseId>,
    /// Bounded diagnostics from status, runtime, and the latest operation.
    pub diagnostics: Vec<DiagnosticView>,
    /// The URL of every route the current runtime target renders, and
    /// whether each is ready; see [`RouteUrl::derive`]. Absent from daemons
    /// that predate URL readiness, which is not the same as no routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<RouteUrl>>,
    /// The private links the current runtime target takes part in. Absent
    /// from daemons that predate links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<crate::links::LinkReport>,
    /// Whether it receives the daemon's telemetry. Absent from daemons that
    /// predate telemetry receivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryReport>,
    /// The volumes it binds its manifest volume names to. Absent from
    /// daemons that predate tracked volumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<super::VolumeView>>,
}

impl EnvironmentDetailView {
    /// Whether the runtime was observed and every service of the current
    /// runtime target is healthy. A target may have no services, so an empty
    /// service list alone does not mean it was observed.
    #[must_use]
    pub fn runtime_ready(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == crate::ApiErrorCode::RuntimeUnavailable)
            && self
                .observed
                .services
                .iter()
                .all(ObservedServiceView::healthy)
    }
}
