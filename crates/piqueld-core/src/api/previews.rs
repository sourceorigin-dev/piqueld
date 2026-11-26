//! Previews of branches, and their `previews` limits.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{
    AcceptedOperation, DeploymentOrigin, DiagnosticView, EnvironmentStatusView, EnvironmentView,
};
use crate::manifest::PreviewLimits;
use crate::{ApplicationId, EnvironmentId, Operation, Preview};

/// Creates a preview of a branch of the application's manifest repository,
/// or returns the existing preview of that branch and slot. Needs no
/// application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePreviewRequest {
    /// Branch to deploy.
    pub branch: String,
    /// Distinguishes several previews of one branch, e.g. one per agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// A preview and its deployment, as created or found.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct CreatedPreview {
    /// The preview.
    pub preview: EnvironmentView,
    /// Its first deployment or, when it already existed, its latest
    /// operation: creating a preview again never redeploys it.
    pub operation: AcceptedOperation,
    /// Whether this request created the preview.
    pub created: bool,
}

/// Where a preview's branch is, read on demand with `git ls-remote`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BranchState {
    /// The branch is at the deployed commit, or nothing was fetched yet.
    Exists {
        /// Commit at the branch head.
        head: String,
    },
    /// The branch moved past the deployed commit.
    Moved {
        /// Commit at the branch head.
        head: String,
        /// Commit the preview last fetched its manifest from.
        deployed: String,
    },
    /// The repository answered and has no such branch.
    Gone,
    /// The preview runs or is deploying a snapshot of a local worktree
    /// (`preview deploy --local`), so the branch may never have been
    /// pushed. Never counts as gone.
    Local {
        /// Commit at the branch head; absent while the repository has no
        /// such branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
    },
    /// The repository could not be read. Never counts as gone.
    Unknown {
        /// Why the repository could not be read.
        message: String,
    },
}

impl std::fmt::Display for BranchState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists { .. } => formatter.write_str("exists"),
            Self::Moved { .. } => formatter.write_str("moved"),
            Self::Gone => formatter.write_str("gone"),
            Self::Local { .. } => formatter.write_str("local"),
            Self::Unknown { .. } => formatter.write_str("unknown"),
        }
    }
}

/// A preview with its status, latest operation, hostnames, and branch state.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PreviewView {
    /// The preview.
    pub preview: EnvironmentView,
    /// Its reconciliation status.
    pub status: EnvironmentStatusView,
    /// Its newest operation, if any.
    pub latest_operation: Option<Operation>,
    /// Hostnames its deployed target routes; none before its first deployment.
    pub hostnames: Vec<String>,
    /// Where its branch is now.
    pub branch: BranchState,
    /// Where what it runs came from, or, until a deployment was prepared,
    /// its latest deployment: built from its branch, or a local snapshot;
    /// `build` before its first. A failed redeployment of its branch keeps
    /// a local snapshot it runs.
    #[serde(default)]
    pub origin: DeploymentOrigin,
    /// How `previews` bounded its deployed target: services that run with
    /// default limits or fewer replicas than their manifest asks for.
    #[serde(default)]
    pub bounds: Vec<DiagnosticView>,
    /// Whether pushes to its branch redeploy it: `following` while its
    /// application syncs and once it was deployed.
    pub sync: crate::sync::SyncState,
}

/// Deletes the listed previews whose branch the repository confirms is gone.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PrunePreviewsRequest {
    /// Previews to delete, as confirmed by the caller. Any whose branch is
    /// not confirmed gone when the request runs is kept.
    pub previews: Vec<EnvironmentId>,
}

/// A preview whose deletion was accepted.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeletedPreview {
    /// The preview.
    pub preview: EnvironmentView,
    /// Its deletion.
    pub operation: AcceptedOperation,
}

/// A `previews` count limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewLimit {
    /// `max_per_application`: the previews of one application.
    PerApplication,
    /// `max_total`: the previews of every application.
    Total,
}

/// Names the setting: `max_per_application` or `max_total`.
impl std::fmt::Display for PreviewLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::PerApplication => "max_per_application",
            Self::Total => "max_total",
        })
    }
}

/// Details of `preview_limit_reached`: the limit creating another preview
/// would exceed, and the previews it counts, so a caller can choose which
/// to delete.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PreviewLimitReached {
    /// The limit reached.
    pub limit: PreviewLimit,
    /// Its configured value.
    pub max: u32,
    /// The previews it counts that the caller may read, oldest deployment
    /// first.
    pub previews: Vec<CountedPreview>,
}

/// A preview counted against a `previews` limit.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct CountedPreview {
    /// Stable preview identifier.
    pub id: EnvironmentId,
    /// Owning application.
    pub application_id: ApplicationId,
    /// Its branch, slot, and slug.
    pub preview: Preview,
    /// Whether its deletion is in progress. It counts until it is gone.
    pub deleting: bool,
    /// Its newest deployment; absent before its first.
    pub last_deployment: Option<LastDeployment>,
}

/// When a deployment was requested.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct LastDeployment {
    /// Deployment (and operation) identifier.
    pub id: String,
    /// Request timestamp in Unix milliseconds.
    pub created_at_ms: i64,
}

/// Previews against the daemon's `previews` limits.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct PreviewUsage {
    /// The configured limits.
    pub limits: PreviewLimits,
    /// Previews of every application, including those being deleted. Above
    /// `limits.max_total` once the limit is lowered: existing previews keep
    /// running, new ones are refused.
    pub total: u32,
    /// Readable applications with previews, in name order.
    pub applications: Vec<ApplicationPreviews>,
    /// CPU limits in millicores, summed over the replicas of each preview's
    /// current target (which the daemon keeps converging Docker to) that have
    /// both limits.
    pub cpu_millis: u64,
    /// Memory limits in bytes, summed over the deployed replicas that have
    /// both limits.
    pub memory_bytes: u64,
    /// Deployed replicas missing a CPU or memory limit: deployed before the
    /// limits applied, until their preview is redeployed.
    #[serde(default)]
    pub unlimited_replicas: u64,
}

/// One application's previews against `max_per_application`.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ApplicationPreviews {
    /// Stable application identifier.
    pub id: ApplicationId,
    /// Editable application name.
    pub name: String,
    /// Its previews; above `max_per_application` once the limit is lowered.
    pub previews: u32,
}
