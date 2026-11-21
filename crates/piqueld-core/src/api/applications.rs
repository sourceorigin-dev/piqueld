//! Applications: their views, saving, renaming and deleting them, and plans.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{EnvironmentSummary, EnvironmentView, ReleasePlan};
use crate::manifest::{
    ApplicationManifest, ApplicationTemplate, RolloutOrder, RolloutOrderSource, VariableValue,
};
use crate::{ApplicationId, EnvironmentName, Operation, Plan};

/// Metadata returned when listing applications.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ApplicationSummary {
    /// Stable application identifier.
    pub id: ApplicationId,
    /// Editable application name.
    pub name: String,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Environments in name order, with their status and latest operation.
    pub environments: Vec<EnvironmentSummary>,
}

/// Public application state returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ApplicationView {
    /// Saved manifest, shared by every environment, with references unresolved.
    pub application: ApplicationTemplate,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Hash of the normalized desired specification.
    pub spec_hash: String,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Environments in name order. Never includes previews.
    pub environments: Vec<EnvironmentView>,
    /// Previews in slug order.
    #[serde(default)]
    pub previews: Vec<EnvironmentView>,
    /// How the environments differ from the ones the saved manifest
    /// declares: changes waiting for its next save or fetch, and
    /// environments it no longer declares, kept until deleted explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<crate::manifest::EnvironmentChange>,
    /// The last time sync listed its repository's branches; absent until it
    /// did, and while sync is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_check: Option<crate::sync::SyncCheck>,
    /// The manifest it last read from its repository, with its connection,
    /// while repository-backed and once it has read one: where the saved
    /// manifest differs from it, it was [edited](Self::edits) since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<ApplicationTemplate>,
}

impl ApplicationView {
    /// Whether the saved manifest was edited since it was last read from
    /// its repository. Its deployments from the branch `spec.manifest` names
    /// keep deploying it until a push changes the manifest there.
    #[must_use]
    pub fn edited(&self) -> bool {
        self.repository
            .as_ref()
            .is_some_and(|repository| repository.spec_hash() != self.spec_hash)
    }

    /// The fields edited since the manifest was last read from its
    /// repository, from the repository's values to the saved ones.
    #[must_use]
    pub fn edits(&self) -> Vec<ManifestChange> {
        self.repository
            .as_ref()
            .map_or_else(Vec::new, |repository| {
                ManifestChange::edits(repository.spec(), self.application.spec())
            })
    }

    /// Selects the environment runtime commands use when none is named: the
    /// application's only environment.
    ///
    /// # Errors
    ///
    /// Returns the environment names when there is not exactly one, so callers
    /// never pick an environment silently.
    pub fn sole_environment(&self) -> Result<&EnvironmentView, Vec<EnvironmentName>> {
        match self.environments.as_slice() {
            [environment] => Ok(environment),
            environments => Err(environments
                .iter()
                .map(|environment| environment.name.clone())
                .collect()),
        }
    }

    /// Finds an environment by stable ID, then by name. IDs win because a name
    /// can equal another environment's ID.
    #[must_use]
    pub fn environment(&self, id_or_name: &str) -> Option<&EnvironmentView> {
        let environments = || self.environments.iter();
        environments()
            .find(|environment| environment.id.as_str() == id_or_name)
            .or_else(|| environments().find(|environment| environment.name.as_str() == id_or_name))
    }

    /// Finds a preview by stable ID, then by slug, then by branch and `slot`,
    /// in that order, since a slug or branch can equal another's ID.
    #[must_use]
    pub fn preview(&self, selector: &str, slot: Option<&str>) -> Option<&EnvironmentView> {
        let previews = || self.previews.iter();
        previews()
            .find(|preview| preview.id.as_str() == selector)
            .or_else(|| previews().find(|preview| preview.name.as_str() == selector))
            .or_else(|| {
                previews().find(|preview| {
                    preview.preview().is_some_and(|identity| {
                        identity.branch.as_str() == selector
                            && identity.slot.as_ref().map(crate::PreviewSlot::as_str) == slot
                    })
                })
            })
    }

    /// Finds an environment or a preview by stable ID.
    #[must_use]
    pub fn deployable(&self, id: &str) -> Option<&EnvironmentView> {
        self.environments
            .iter()
            .chain(&self.previews)
            .find(|deployable| deployable.id.as_str() == id)
    }
}

/// Desired application state to apply or preview.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyApplicationRequest {
    /// Application configuration to save or preview. Deployment is an explicit query option.
    pub manifest: ApplicationManifest,
    /// Required for apply unless forced; optional for preview. Zero requires an absent name.
    #[serde(default)]
    pub expected_generation: Option<u64>,
    /// Required for non-forced updates by name; protects against name reuse.
    #[serde(default)]
    pub expected_application_id: Option<String>,
}

/// Operation accepted by a mutation endpoint.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AcceptedOperation {
    /// Asynchronous operation identifier.
    pub operation_id: String,
    /// Environment the operation changes.
    pub environment_id: String,
    /// Accepted intent revision.
    pub generation: u64,
}

/// Dry-run plan returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PlanView {
    /// Stable application identifier.
    pub application_id: String,
    /// Current intent revision; zero means the name is absent.
    pub generation: u64,
    /// Whether this specification matches the baseline: the latest deployment
    /// snapshot of the selected environment (by default the application's only
    /// one), or the saved configuration when none is selected and it has several.
    pub identical: bool,
    /// Latest operation of the selected environment at the time of comparison.
    pub operation: Option<Operation>,
    /// Safe changes relative to the baseline.
    pub changes: Vec<ManifestChange>,
    /// How saving the manifest would change the application's environments
    /// (creating, re-sourcing, opting into sync), and the environments it no
    /// longer declares. Empty for release plans, and when nothing changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<crate::manifest::EnvironmentChange>,
    /// Ordered runtime plan for the selected environment; unresolved images are
    /// explicit actions. Empty when no environment is selected.
    pub plan: Plan,
    /// How much of the runtime change `plan` lists.
    #[serde(default)]
    pub runtime_coverage: RuntimeCoverage,
    /// The value of every variable in scope for the selected environment,
    /// keyed by reference, e.g. `vars.domain`. Empty when none is selected.
    #[serde(default)]
    pub variables: BTreeMap<String, VariableValue>,
    /// Effective rollout of each service, sorted by service name.
    pub rollouts: Vec<ServiceRolloutView>,
    /// For a plan of a release (`env promote --plan`):
    /// the release, its origin, new volumes, and unusable secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleasePlan>,
}

/// How much of the runtime change a plan lists.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCoverage {
    /// Every runtime action the change needs, as observed when planning.
    #[default]
    Complete,
    /// Sources must be resolved or built first: the plan lists that and
    /// the declared jobs, while network, volume, and service changes are
    /// planned once the deployment has resolved the sources.
    Resolution,
    /// None: without a selected environment, a plan compares saved
    /// manifests only.
    Omitted,
}

impl RuntimeCoverage {
    /// The coverage of the runtime plan of the selected environment, or
    /// [`Self::Omitted`] without one.
    #[must_use]
    pub fn of(plan: Option<&Plan>) -> Self {
        match plan {
            None => Self::Omitted,
            Some(plan)
                if plan
                    .actions
                    .iter()
                    .any(|action| action.reason == crate::ActionReason::ResolutionRequired) =>
            {
                Self::Resolution
            }
            Some(_) => Self::Complete,
        }
    }

    /// What a plan leaves out, for people; `None` when it is complete.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::Complete => None,
            Self::Resolution => Some(
                "Network, volume, and service changes are not listed yet: the deployment plans them once it has resolved the sources. Source resolution and declared jobs are listed.",
            ),
            Self::Omitted => Some(
                "Runtime actions are not planned: select an environment to compare with its runtime.",
            ),
        }
    }
}

/// A service's effective rollout order and monitor window.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ServiceRolloutView {
    /// Logical service name.
    pub service: String,
    /// Effective update order.
    pub order: RolloutOrder,
    /// Whether the order is set explicitly or derived from the mounts.
    pub order_source: RolloutOrderSource,
    /// Effective monitor window in seconds.
    pub monitor_seconds: u32,
}

/// A change to a manifest field. Environment and process values are redacted.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ManifestChange {
    /// Logical manifest path.
    pub field: String,
    /// Safe previous value; absent for additions.
    pub before: Option<String>,
    /// Safe proposed value; absent for removals.
    pub after: Option<String>,
}

/// Summarizes a newly accepted operation for mutation responses.
impl From<&Operation> for AcceptedOperation {
    fn from(operation: &Operation) -> Self {
        Self {
            operation_id: operation.id.clone(),
            environment_id: operation.environment_id.to_string(),
            generation: operation.generation,
        }
    }
}

/// Accepted application deletion: one delete operation per environment,
/// previews included.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeletedApplication {
    /// Stable application identity.
    pub application_id: String,
    /// Revision after requesting deletion.
    pub generation: u64,
    /// Deletion of each environment and preview; the application disappears
    /// with the last one.
    pub operations: Vec<AcceptedOperation>,
}

/// Saved application configuration, optionally accompanied by a deployment.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct SavedApplication {
    /// Stable application identity.
    pub application_id: String,
    /// Saved configuration revision.
    pub generation: u64,
    /// The first deployment operation, only when explicitly requested; see
    /// `operation_ids` for applications that declare several environments.
    pub operation_id: Option<String>,
    /// Every deployment operation started, one per environment: the
    /// application's only one, or each declared environment that builds.
    #[serde(default)]
    pub operation_ids: Vec<String>,
}

impl SavedApplication {
    /// Every deployment operation started. Daemons from before
    /// `operation_ids` report only `operation_id`.
    #[must_use]
    pub fn deployments(&self) -> Vec<&str> {
        if self.operation_ids.is_empty() {
            self.operation_id.iter().map(String::as_str).collect()
        } else {
            self.operation_ids.iter().map(String::as_str).collect()
        }
    }
}
