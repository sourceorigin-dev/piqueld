//! Deployments, releases, and promoting or restoring them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::openapi::schema::{AnyOfBuilder, Schema};
use utoipa::openapi::{RefOr, schema::RefBuilder};
use utoipa::{PartialSchema, ToSchema};

use super::{AcceptedOperation, DiagnosticView, Page};
use crate::manifest::{ApplicationTemplate, SystemVariable, VariableValue};
use crate::names::string_enum;
use crate::{
    ApplicationId, BuildFingerprint, EnvironmentId, NormalizedApplication, Operation, Release,
    ReleaseId, Sha256Digest,
};

/// Durable deployment snapshot and execution summary.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeploymentView {
    /// Execution ID also identifies this deployment.
    pub operation: Operation,
    /// Manifest captured for this deployment, with references unresolved.
    pub template: ApplicationTemplate,
    /// Values its references rendered to, keyed by reference.
    pub variables: BTreeMap<String, VariableValue>,
    /// Rendered configuration; absent until a repository-backed manifest is fetched.
    pub application: Option<NormalizedApplication>,
    /// Problems found while fetching the manifest that did not stop the
    /// deployment, e.g. `manifest_connection_ignored`.
    #[serde(default)]
    pub warnings: Vec<DiagnosticView>,
    /// The release its prepared target runs; absent until prepared, and for
    /// deployments that recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseId>,
    /// Where it came from: built from its environment's source, a promoted
    /// or redeployed release, or a local snapshot uploaded to a preview.
    #[serde(default)]
    pub origin: DeploymentOrigin,
    /// First successful convergence, retained during later drift repair.
    pub succeeded_at_ms: Option<i64>,
    /// Whether this is the currently promoted runtime target.
    pub current_target: bool,
    /// Whether this is the most recent deployment to converge successfully.
    pub last_successful: bool,
}

impl DeploymentView {
    /// A system variable's rendered value, e.g. `git.sha`: absent before a
    /// repository-backed manifest is fetched, and for saved manifests.
    #[must_use]
    pub fn system_value(&self, variable: crate::manifest::SystemVariable) -> Option<&str> {
        match self.variables.get(variable.as_str())? {
            VariableValue::String(value) => Some(value),
            VariableValue::Boolean(_) | VariableValue::Integer(_) => None,
        }
    }
}

string_enum! {
    #[serde]
    /// How much of each deployment a deployment listing returns.
    pub enum DeploymentListView {
        /// Whole snapshots, with their manifests: [`DeploymentView`]s.
        Full => "full",
        /// What a history row shows, without manifests:
        /// [`DeploymentSummary`]s.
        Summary => "summary",
    }
}

/// A deployment without its manifests, as a history row shows it.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeploymentSummary {
    /// Execution ID also identifies this deployment.
    pub operation: Operation,
    /// Where it came from.
    #[serde(default)]
    pub origin: DeploymentOrigin,
    /// The release its prepared target runs; absent until prepared, and for
    /// deployments that recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseId>,
    /// First successful convergence, retained during later drift repair.
    pub succeeded_at_ms: Option<i64>,
    /// Whether this is the currently promoted runtime target.
    pub current_target: bool,
    /// Whether this is the most recent deployment to converge successfully.
    pub last_successful: bool,
    /// The commit its repository manifest was read from (`git.sha`); absent
    /// for a saved manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    /// The branch its repository manifest was read from (`git.branch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

impl From<DeploymentView> for DeploymentSummary {
    fn from(view: DeploymentView) -> Self {
        let value = |variable| view.system_value(variable).map(str::to_owned);
        Self {
            git_sha: value(SystemVariable::GitSha),
            git_branch: value(SystemVariable::GitBranch),
            operation: view.operation,
            origin: view.origin,
            release: view.release,
            succeeded_at_ms: view.succeeded_at_ms,
            current_target: view.current_target,
            last_successful: view.last_successful,
        }
    }
}

/// A page of an environment's deployments, newest first, in the
/// [`DeploymentListView`] asked for.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum DeploymentPage {
    /// Whole snapshots.
    Full(Page<DeploymentView>),
    /// Summaries. An empty page reads as `Full`.
    Summary(Page<DeploymentSummary>),
}

impl DeploymentPage {
    /// The component name of a page of `T`s, as utoipa names generic schemas.
    fn page_name<T: ToSchema>() -> String {
        format!("{}_{}", Page::<T>::name(), T::name())
    }

    /// A reference to the page of `T`s, described as `description`.
    fn page_ref<T: ToSchema>(description: &str) -> RefOr<Schema> {
        RefOr::Ref(
            RefBuilder::new()
                .ref_location_from_schema_name(Self::page_name::<T>())
                .description(Some(description))
                .build(),
        )
    }

    /// Registers the page of `T`s, `T` and the schemas they use.
    fn page_schemas<T: ToSchema>(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        schemas.push((Self::page_name::<T>(), <Page<T> as PartialSchema>::schema()));
        schemas.push((T::name().into_owned(), T::schema()));
        <Page<T> as ToSchema>::schemas(schemas);
        T::schemas(schemas);
    }
}

// Written out because the derive documents an untagged enum as `oneOf`, which
// a whole page fails: it also matches the summary schema, which allows more
// properties, and an empty page matches both. `anyOf` accepts either.
impl PartialSchema for DeploymentPage {
    fn schema() -> RefOr<Schema> {
        AnyOfBuilder::new()
            .description(Some(
                "A page of an environment's deployments, newest first, in the\n\
                 [`DeploymentListView`] asked for.",
            ))
            .item(Self::page_ref::<DeploymentView>("Whole snapshots."))
            .item(Self::page_ref::<DeploymentSummary>(
                "Summaries. An empty page reads as `Full`.",
            ))
            .into()
    }
}

impl ToSchema for DeploymentPage {
    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        Self::page_schemas::<DeploymentView>(schemas);
        Self::page_schemas::<DeploymentSummary>(schemas);
    }
}

/// An immutable release of an application, recorded by a successful
/// preparation in one of its tracking environments. Environments whose
/// deployments prepared the same content share it.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ReleaseView {
    /// Stable release identifier.
    pub id: ReleaseId,
    /// Owning application.
    pub application_id: ApplicationId,
    /// Hash of its content, unique within the application.
    pub content_hash: Sha256Digest,
    /// When it was first recorded, in Unix milliseconds.
    pub created_at_ms: i64,
    /// Its manifest, commit, and each service's provenance and image.
    pub release: Release,
    /// The build inputs each service's image was prepared from.
    pub fingerprint: BuildFingerprint,
    /// Whether its images are still present, so it can be deployed again;
    /// absent when Docker could not be asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<crate::ReleaseAvailability>,
    /// Every deployment that received it rather than building it, newest
    /// first: where it was promoted or deployed again.
    #[serde(default)]
    pub promotions: Vec<ReleasePromotion>,
}

/// A deployment that received a release: promoted, deployed again by ID, or
/// restored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ReleasePromotion {
    /// The environment it was deployed to.
    pub environment_id: EnvironmentId,
    /// That deployment, whose ID is also its operation's.
    pub deployment_id: String,
    /// Where it came from; never `build`.
    pub origin: DeploymentOrigin,
    /// When it was accepted, in Unix milliseconds.
    pub created_at_ms: i64,
}

/// Where a deployment came from.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentOrigin {
    /// Built from its environment's source: the saved manifest or its branch.
    /// It records its release once prepared.
    #[default]
    Build,
    /// The release of another environment's deployment, promoted into this one.
    Promotion {
        /// The environment it was promoted from.
        environment: EnvironmentId,
        /// That environment's deployment whose release it runs.
        deployment: String,
    },
    /// An earlier release, deployed again by ID.
    Release,
    /// An earlier deployment of the same environment, restored exactly: its
    /// rendered manifest, images and pinned secret versions. It never runs
    /// jobs.
    Restore {
        /// The deployment it restored.
        deployment: String,
        /// The deployment whose failed rollout piqueld rolled back by
        /// restoring, in an environment with automatic rollback. Absent when
        /// someone asked for the restore.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rolled_back: Option<String>,
    },
    /// A snapshot of a local worktree, uploaded to a preview with
    /// `preview deploy --local`, and built like its branch would be.
    Local(LocalSnapshot),
}

/// A snapshot of a local worktree: a commit of its committed, uncommitted,
/// and untracked files, except ignored ones, on top of its `HEAD`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct LocalSnapshot {
    /// The snapshot commit, whose parent is the worktree's `HEAD`.
    pub commit: String,
    /// The commit of the application's repository it was uploaded against:
    /// the nearest ancestor of `HEAD` the repository has. Absent when the
    /// whole history was uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// What `env promote` deploys into a promoted environment. Either way the
/// release is pinned when the promotion is accepted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum PromotionSelection {
    /// The release of the source environment's current deployment, which
    /// must be `deployment` when given.
    Source {
        /// The deployment the caller reviewed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deployment: Option<String>,
    },
    /// An earlier release of the application, rendered with the
    /// environment's current secrets.
    Release {
        /// The release to deploy.
        release: ReleaseId,
    },
}

/// Promotes a release into a promoted environment, conditioned on the
/// inspected application revision. By default the source environment's
/// current deployment; `deployment` and `release` are mutually exclusive.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PromoteRequest {
    /// The source deployment to promote; it must still be the source's current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    /// An earlier release to deploy instead of the source's current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Current application revision, required unless explicitly forced; ignored by plans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
}

/// Restores an earlier deployment of an environment or preview exactly. No
/// application revision is needed: the deployment pins everything deployed.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    /// The deployment to restore; it must have succeeded.
    pub deployment: String,
}

/// An accepted promotion and the release it pinned.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AcceptedPromotion {
    /// The deployment operation; its ID is also the deployment's.
    #[serde(flatten)]
    pub operation: AcceptedOperation,
    /// The release it deploys.
    pub release_id: ReleaseId,
    /// Where the release came from.
    pub origin: DeploymentOrigin,
}

/// A secret a rendered deployment mounts but cannot use. Promotions are
/// refused while any is left. Generated secrets are generated when missing,
/// so only a pending deletion makes one unusable.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum SecretProblem {
    /// Mounted from the application's store, which has no such secret.
    Missing {
        /// Logical secret name.
        secret: String,
    },
    /// Stored, but its access list excludes the environment.
    AccessDenied {
        /// Logical secret name.
        secret: String,
    },
    /// Stored, but key recovery discarded its value; store a new version.
    Unavailable {
        /// Logical secret name.
        secret: String,
    },
    /// Being deleted, stored or generated; finish or abandon the deletion.
    Deleting {
        /// Logical secret name.
        secret: String,
    },
}

/// What a plan of a release adds: the release and where it comes from, and
/// what the environment still lacks.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ReleasePlan {
    /// The release, with its provenance and image availability.
    pub release: ReleaseView,
    /// Where it comes from.
    pub origin: DeploymentOrigin,
    /// Volumes it declares that the environment doesn't have yet; they are
    /// created empty.
    pub new_volumes: Vec<String>,
    /// Every secret it mounts that is missing or that the environment may not use.
    pub secrets: Vec<SecretProblem>,
}
