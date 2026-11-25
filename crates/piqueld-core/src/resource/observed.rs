//! What Docker reports about the resources piqueld manages, and how it
//! compares with the desired ones.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[cfg(doc)]
use super::TELEMETRY_TARGET;
use super::{
    DesiredFile, DesiredMount, DesiredNetwork, DesiredService, InstanceId, LINK_LABEL,
    OwnershipState, ResolvedApplication, SERVICE_LABEL, SecretFile, TelemetryMount,
};
use crate::manifest::{
    RolloutPolicy, ServiceSecurity, ValidatedHealthCheck, ValidatedResourceLimits,
};
use crate::{EnvironmentId, ResourceKind};

/// Untrusted mount observation from Docker, including foreign volume names.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservedMount {
    /// Docker-reported volume name.
    pub volume_name: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// Untrusted observation of a config mounted as a file.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedFile {
    /// Docker-reported config name.
    pub config_name: String,
    /// Container target path.
    pub target: String,
    /// Permission bits.
    pub mode: u32,
}

impl From<&DesiredFile> for ObservedFile {
    fn from(value: &DesiredFile) -> Self {
        Self {
            config_name: value.config_name.to_string(),
            target: value.target.clone(),
            mode: value.mode.bits(),
        }
    }
}

/// A service's attachment to one network and the DNS aliases it answers to there.
///
/// Observed attachments carry untrusted Docker-reported names; desired ones are
/// derived by [`DesiredService::network_attachments`].
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkAttachment {
    /// Network name.
    pub network: String,
    /// Sorted DNS aliases for the service on this network.
    pub aliases: Vec<String>,
}

impl From<&DesiredMount> for ObservedMount {
    fn from(value: &DesiredMount) -> Self {
        Self {
            volume_name: value.volume_name.to_string(),
            target: value.target.clone(),
            read_only: value.read_only,
        }
    }
}

/// Lifecycle state of an observed Docker task.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// The task has been created but not scheduled.
    New,
    /// The task is waiting for scheduling.
    Pending,
    /// The task has been assigned to a node.
    Assigned,
    /// The node accepted the task.
    Accepted,
    /// The task is preparing its container.
    Preparing,
    /// The container is starting.
    Starting,
    /// The container is running.
    Running,
    /// The task completed successfully.
    Complete,
    /// The task failed.
    Failed,
    /// The task was rejected before starting.
    Rejected,
    /// The task was shut down.
    Shutdown,
    /// Docker did not provide a recognized state.
    #[default]
    #[serde(other)]
    Unknown,
}

impl TaskState {
    /// Whether the task's container is up, possibly still waiting on its
    /// healthcheck.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }

    /// Whether the task is over: it completed, failed, was rejected, or was
    /// shut down.
    #[must_use]
    pub const fn ended(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Rejected | Self::Shutdown
        )
    }
}

/// Sanitized observation of one Docker task.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedTask {
    /// Docker task ID.
    #[serde(default)]
    pub id: String,
    /// The replica slot the task fills.
    #[serde(default)]
    pub slot: Option<u64>,
    /// Whether the task runs the service's current specification, rather
    /// than one an update is replacing.
    #[serde(default)]
    pub current: bool,
    /// Current Docker task state.
    pub state: TaskState,
    /// Backend health result, when available.
    pub healthy: Option<bool>,
    /// Whether the task is still desired by the service.
    pub desired_running: bool,
    /// Sanitized task failure information.
    pub diagnostic: Option<TaskDiagnostic>,
}

/// Sanitized diagnostic for a failed task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskDiagnostic {
    /// The container exited with an optional exit code.
    Failed {
        /// Exit code reported by Docker, when available.
        exit_code: Option<i64>,
    },
    /// Docker rejected the task before it could run.
    Rejected,
}

/// Aggregate health state derived from observed tasks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Convergence {
    /// All desired tasks are healthy and running.
    Converged,
    /// Docker is still applying an update.
    Updating,
    /// Some desired tasks are healthy but others are not.
    Degraded,
    /// No desired task is healthy or the update is paused.
    Failed,
}

/// Observed Docker network state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedNetwork {
    /// Observed Docker network name.
    pub name: String,
    /// Whether adapter-owned network settings remain canonical.
    pub runtime_configuration_matches: bool,
    /// Ownership labels observed on the network.
    pub labels: BTreeMap<String, String>,
    /// Whether containers are attached to the network on this node.
    #[serde(default)]
    pub in_use: bool,
}

impl ObservedNetwork {
    /// Whether this is a link network services are still attached to.
    #[must_use]
    pub fn is_linked_in_use(&self) -> bool {
        self.in_use && self.labels.contains_key(LINK_LABEL)
    }

    /// Returns whether ownership labels identify the desired network.
    ///
    /// Only checks identity (owner and name), not configuration drift.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredNetwork,
        application: &ResolvedApplication,
    ) -> bool {
        self.is_owned_by(&application.instance_id, &application.id)
            && self.name == desired.name.as_str()
    }

    /// Returns whether the labels, role, and canonical name identify this network.
    #[must_use]
    pub fn is_owned_by(&self, instance: &InstanceId, application: &EnvironmentId) -> bool {
        OwnershipState::for_resource(
            &self.labels,
            instance,
            application,
            ResourceKind::Network,
            &self.name,
        ) == OwnershipState::Owned
    }
}

/// Observed Docker volume state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedVolume {
    /// Observed Docker volume name.
    pub name: String,
    /// Whether the backend volume uses piqueld's supported local driver.
    pub runtime_configuration_matches: bool,
    /// Ownership labels observed on the volume.
    pub labels: BTreeMap<String, String>,
}

impl ObservedVolume {
    /// Returns whether this volume has the expected owner and volume label role.
    #[must_use]
    pub fn is_owned_by(&self, instance: &InstanceId, application: &EnvironmentId) -> bool {
        OwnershipState::for_resource(
            &self.labels,
            instance,
            application,
            ResourceKind::Volume,
            &self.name,
        ) == OwnershipState::Owned
    }
}

/// Observed Docker service state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedService {
    /// Observed Docker service name.
    pub name: String,
    /// Observed digest-pinned image.
    pub image: String,
    /// Observed replica count.
    pub replicas: u16,
    /// Environment variables observed in the container spec.
    pub environment: BTreeMap<String, String>,
    /// Observed container command.
    pub command: Vec<String>,
    /// Observed command arguments.
    pub arguments: Vec<String>,
    /// Persistent mounts observed on the service.
    pub mounts: Vec<ObservedMount>,
    /// Immutable secret file bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretFile>,
    /// Configs mounted as files, except piqueld's health probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ObservedFile>,
    /// Observed health check.
    pub healthcheck: Option<ValidatedHealthCheck>,
    /// Whether Docker has a health check, including an unsupported one.
    pub healthcheck_configured: bool,
    /// Observed resource limits.
    pub resources: Option<ValidatedResourceLimits>,
    /// Networks attached to the service and their aliases.
    pub networks: Vec<NetworkAttachment>,
    /// Ownership labels observed on the service.
    pub labels: BTreeMap<String, String>,
    /// Observed update policy; `None` when Docker reports one piqueld cannot express.
    pub rollout: Option<RolloutPolicy>,
    /// The bind mount at [`TELEMETRY_TARGET`] and the container's user, if
    /// either is set; the user is empty without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryMount>,
    /// Observed security settings; the user is the telemetry receiver's
    /// while its socket is mounted, so it appears there instead.
    #[serde(default, skip_serializing_if = "ServiceSecurity::is_default")]
    pub security: ServiceSecurity,
    /// Whether adapter-owned settings remain canonical.
    pub runtime_configuration_matches: bool,
    /// Task observations used to derive convergence.
    pub tasks: Vec<ObservedTask>,
    /// Aggregate convergence state.
    pub convergence: Convergence,
}

impl ObservedService {
    /// Whether observed mounts equal the desired ones, ignoring order.
    pub(crate) fn mounts_match(&self, desired: &DesiredService) -> bool {
        unordered_eq(
            self.mounts.iter().cloned(),
            desired.mounts.iter().map(ObservedMount::from),
        )
    }

    /// Whether network attachments, including their aliases, equal the desired
    /// ones, ignoring order but not multiplicity.
    pub(crate) fn networks_match(&self, desired: &DesiredService) -> bool {
        unordered_eq(&self.networks, &desired.network_attachments())
    }

    /// Compares health checks by what Docker executes, so an HTTP check and its
    /// equivalent `piqueld-probe` command match. An unsupported observed check
    /// (configured but not representable) never matches.
    pub(crate) fn healthcheck_matches(&self, desired: &DesiredService) -> bool {
        self.healthcheck
            .as_ref()
            .map(ValidatedHealthCheck::execution)
            == desired
                .healthcheck
                .as_ref()
                .map(ValidatedHealthCheck::execution)
            && self.healthcheck_configured == desired.healthcheck.is_some()
    }

    /// Returns whether all desired service fields match.
    #[must_use]
    pub fn matches(&self, desired: &DesiredService) -> bool {
        self.drift(desired).is_empty()
    }

    /// Lists the desired field names that differ from this observation, as
    /// reported by `ActionReason::Drift`.
    ///
    /// ```text
    /// image changed and a replica added -> ["image", "replicas"]
    /// ```
    #[must_use]
    pub fn drift(&self, desired: &DesiredService) -> Vec<&'static str> {
        [
            ("image", self.image == desired.image.as_str()),
            ("replicas", self.replicas == desired.replicas),
            ("environment", self.environment == desired.environment),
            ("command", self.command == desired.command),
            ("arguments", self.arguments == desired.arguments),
            ("secrets", unordered_eq(&self.secrets, &desired.secrets)),
            (
                "files",
                unordered_eq(
                    self.files.iter().cloned(),
                    desired.files.iter().map(ObservedFile::from),
                ),
            ),
            ("mounts", self.mounts_match(desired)),
            ("networks", self.networks_match(desired)),
            ("healthcheck", self.healthcheck_matches(desired)),
            ("resources", self.resources == desired.resources),
            ("rollout", self.rollout == Some(desired.rollout_policy())),
            ("telemetry", self.telemetry == desired.telemetry),
            ("security", self.security == desired.security),
            ("runtime_policy", self.runtime_configuration_matches),
            ("labels", owned_label_subset(&self.labels, &desired.labels)),
        ]
        .into_iter()
        .filter_map(|(field, matches)| (!matches).then_some(field))
        .collect()
    }

    /// Returns whether ownership labels identify the desired service.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredService,
        application: &ResolvedApplication,
    ) -> bool {
        self.is_owned_by(&application.instance_id, &application.id)
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(desired.logical_name.as_str())
            && self.name == desired.name.as_str()
    }

    /// Returns whether labels and the canonical name identify this service.
    ///
    /// Unlike `matches_ownership`, needs no desired service: it recomputes the
    /// canonical name from the service label, so it can classify obsolete
    /// services during cleanup and deletion.
    #[must_use]
    pub fn is_owned_by(&self, instance: &InstanceId, application: &EnvironmentId) -> bool {
        OwnershipState::for_resource(
            &self.labels,
            instance,
            application,
            ResourceKind::Service,
            &self.name,
        ) == OwnershipState::Owned
    }
}

/// Observed resources associated with an application.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedApplication {
    /// Networks observed for the application.
    pub networks: Vec<ObservedNetwork>,
    /// Volumes observed for the application.
    pub volumes: Vec<ObservedVolume>,
    /// Services observed for the application.
    pub services: Vec<ObservedService>,
    /// Link networks other environments own for their links to this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_networks: Vec<ObservedNetwork>,
}

impl ObservedApplication {
    /// Attachments of `environment`'s services to link networks that none of
    /// `granted` grants them, as service and network names. `granted` are
    /// the environment's active and prepared targets projected with its
    /// current links; none while it is being deleted.
    ///
    /// These are detached as soon as they are found, not by a rollout: an
    /// unrelated failing service or a running deployment must not keep a
    /// revoked link open.
    #[must_use]
    pub fn revoked_links(
        &self,
        instance: &InstanceId,
        environment: &EnvironmentId,
        granted: &[ResolvedApplication],
    ) -> Vec<(&str, &str)> {
        let link_networks = self
            .linked_networks
            .iter()
            .chain(
                self.networks
                    .iter()
                    .filter(|network| network.labels.contains_key(LINK_LABEL)),
            )
            .map(|network| network.name.as_str())
            .collect::<BTreeSet<_>>();
        self.services
            .iter()
            .filter(|service| service.is_owned_by(instance, environment))
            .flat_map(|service| {
                service
                    .networks
                    .iter()
                    .filter(|attachment| link_networks.contains(attachment.network.as_str()))
                    .filter(move |attachment| {
                        !granted
                            .iter()
                            .flat_map(|target| &target.services)
                            .filter(|desired| desired.name.as_str() == service.name)
                            .flat_map(|desired| &desired.networks)
                            .any(|network| network.as_str() == attachment.network)
                    })
                    .map(|attachment| (service.name.as_str(), attachment.network.as_str()))
            })
            .collect()
    }
}

/// Multiset equality: same elements with the same counts, in any order.
pub(crate) fn unordered_eq<T: Ord>(
    observed: impl IntoIterator<Item = T>,
    desired: impl IntoIterator<Item = T>,
) -> bool {
    let mut observed: Vec<_> = observed.into_iter().collect();
    let mut desired: Vec<_> = desired.into_iter().collect();
    observed.sort();
    desired.sort();
    observed == desired
}

/// Whether observed labels contain every desired label and no extra or
/// differing `io.piqueld.*` labels.
///
/// Foreign labels added by Docker or operators are tolerated.
pub(crate) fn owned_label_subset(
    observed: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> bool {
    desired
        .iter()
        .all(|(key, value)| observed.get(key) == Some(value))
        && observed
            .iter()
            .filter(|(key, _)| key.starts_with("io.piqueld."))
            .all(|(key, value)| desired.get(key) == Some(value))
}
