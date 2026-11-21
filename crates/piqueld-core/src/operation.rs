//! Shared asynchronous operation records.

use crate::EnvironmentId;
use crate::names::string_enum;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

string_enum! {
    #[serde]
    /// Environment change being reconciled.
    /// Deployment operations retain the serialized kind `refresh` for compatibility
    /// with existing stored records and API clients.
    pub enum OperationKind {
        /// Apply the requested application state.
        Apply => "apply",
        /// Deploy without changing the generation: resolve the current manifest
        /// again, or run a promoted or earlier release. Used by deploys, saves
        /// and edits that deploy, sync, previews, and promotions.
        Refresh => "refresh",
        /// Delete its resources.
        Delete => "delete",
    }
}

string_enum! {
    #[serde]
    /// Lifecycle of one desired-state operation.
    pub enum OperationState {
        /// Accepted and waiting to run.
        Requested => "requested",
        /// Reconciling and verifying runtime state.
        Running => "running",
        /// Runtime state matches the requested state.
        Succeeded => "succeeded",
        /// Reconciliation failed.
        Failed => "failed",
        /// Cancelled without a replacement operation.
        Cancelled => "cancelled",
        /// Replaced by newer intent; callers may stop waiting successfully.
        Superseded => "superseded",
    }
}

impl OperationState {
    /// Whether this attempt has finished.
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }

    /// Whether an operation can make the requested lifecycle transition.
    ///
    /// Staying in the same state is always allowed. `Running` may return to
    /// `Requested` for a new attempt, and `Succeeded`, `Failed`, or `Cancelled`
    /// may be requeued; `Superseded` is final.
    ///
    /// ```text
    /// Requested -> Running | Cancelled | Superseded
    /// Running   -> Requested | Succeeded | Failed | Cancelled | Superseded
    /// Succeeded | Failed | Cancelled -> Requested
    /// ```
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Requested,
                    Self::Running | Self::Cancelled | Self::Superseded
                ) | (
                    Self::Running,
                    Self::Requested
                        | Self::Succeeded
                        | Self::Failed
                        | Self::Cancelled
                        | Self::Superseded
                ) | (
                    Self::Succeeded | Self::Failed | Self::Cancelled,
                    Self::Requested
                )
            )
    }
}

/// Durable asynchronous operation returned by the API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Operation {
    /// Stable operation identifier.
    pub id: String,
    /// Environment the operation changes.
    pub environment_id: EnvironmentId,
    /// Requested change.
    pub kind: OperationKind,
    /// Intent revision this operation targets.
    pub generation: u64,
    /// Number of execution attempts started, including interrupted attempts.
    pub attempt: u64,
    /// Consecutive failed attempts since the last successful execution.
    #[serde(default)]
    pub consecutive_failures: u64,
    /// Current lifecycle state.
    pub state: OperationState,
    /// The phase of the latest step the current attempt journaled, retained
    /// on failure: an action, or a recorded plan (`preparing`, `planning`).
    /// Absent while the operation is requested.
    #[serde(default)]
    pub phase: Option<Phase>,
    /// The resource of that step: a service's manifest name, another
    /// resource's Docker name, or what blocks a recorded plan.
    #[serde(default)]
    pub resource: Option<String>,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
}

impl Operation {
    /// Whether `attempt` is the operation's current run. A timeline shows
    /// only that: running an operation again, e.g. to start a stopped
    /// environment, starts over, and while it is requested again its latest
    /// attempt is the run before.
    pub(crate) fn current_attempt(&self, attempt: u64) -> bool {
        attempt > self.attempt
            || (attempt == self.attempt && self.state != OperationState::Requested)
    }

    /// The attempts to list for this deployment operation, newest first,
    /// from `retained`, the outcomes the API keeps, newest first: the
    /// attempt it is running leads until its outcome is retained. The
    /// dashboard and `piquelctl env attempts` list attempts this way.
    #[must_use]
    pub fn attempts(&self, retained: &[Self]) -> Vec<Self> {
        let running = self.attempt > 0 && !retained.iter().any(|item| item.attempt == self.attempt);
        running
            .then(|| self.clone())
            .into_iter()
            .chain(retained.iter().cloned())
            .collect()
    }

    /// How the operation failed, as `code: message`, either one alone, or
    /// empty without a failure; attempt lists show it as the outcome.
    #[must_use]
    pub fn failure(&self) -> String {
        match (&self.error_code, &self.error_message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(text), None) | (None, Some(text)) => text.clone(),
            (None, None) => String::new(),
        }
    }
}

string_enum! {
    #[serde]
    /// Current environment reconciliation status.
    pub enum ApplicationState {
        /// Configuration is saved but no deployment has been requested.
        NotDeployed => "not_deployed",
        /// Desired state is waiting to be reconciled.
        Pending => "pending",
        /// Runtime resources are being reconciled.
        Deploying => "deploying",
        /// Runtime state matches desired state.
        Ready => "ready",
        /// Runtime state is not fully healthy.
        Degraded => "degraded",
        /// Runtime resources are being deleted.
        Deleting => "deleting",
        /// Reconciliation failed.
        Failed => "failed",
    }
}

string_enum! {
    #[serde]
    /// What a journaled runtime action does, recorded on its events and on the
    /// operation it belongs to. The stored names predate this type and stay as
    /// they are, so history and analytics group old and new actions together.
    #[derive(Hash)]
    pub enum Phase {
        /// Prepare an operation's target: everything before planning.
        Preparing => "preparing",
        /// Fetch a repository-backed manifest.
        FetchManifest => "fetching_manifest",
        /// Build a service's Git source.
        BuildGit => "building_git",
        /// Resolve a service's image to an immutable digest.
        ResolveImage => "resolving_image",
        /// Plan the runtime changes from a fresh observation.
        Planning => "planning",
        /// Run a before-rollout job to completion.
        RunJob => "run_job",
        /// Stop and remove the job runs of earlier operations.
        RemoveJobs => "remove_jobs",
        /// Create or verify a private network.
        EnsureNetwork => "ensure_network",
        /// Create or verify a named volume.
        EnsureVolume => "ensure_volume",
        /// Create or update a service.
        EnsureService => "ensure_service",
        /// Wait for a service to converge.
        WaitForService => "wait_for_service",
        /// Wait for a removed service to disappear.
        WaitForServiceRemoval => "wait_for_service_removal",
        /// Remove a service.
        RemoveService => "remove_service",
        /// Remove a private network.
        RemoveNetwork => "remove_network",
        /// Keep a volume by retention policy.
        RetainVolume => "retain_volume",
        /// Detach a service from a link network.
        DetachNetwork => "detach_network",
        /// Remove a preview's volumes.
        RemoveVolumes => "remove_volumes",
        /// Remove Docker versions of secrets.
        RemoveSecrets => "remove_secrets",
        /// Apply the environment's routes.
        Routing => "routing",
        /// Observe the environment after a failed observation.
        Observation => "observation",
        /// Initialize or repair the local Swarm.
        EnsureSwarm => "ensure_swarm",
        /// Remove an unused image.
        RemoveImage => "remove_image",
        /// Delete from the build registry an image a build pushed.
        DeleteRegistryImage => "delete_registry_image",
        /// Create the managed ingress gateway's network.
        IngressCreateNetwork => "ingress_create_network",
        /// Remove the network of the disabled managed ingress gateway.
        IngressRemoveNetwork => "ingress_remove_network",
        /// Start the managed ingress gateway.
        IngressStartGateway => "ingress_start_gateway",
        /// Stop the managed ingress gateway.
        IngressStopGateway => "ingress_stop_gateway",
        /// Remove a managed ingress container.
        IngressRemoveContainer => "ingress_remove_container",
        /// Pull the managed ingress gateway's image.
        IngressPullImage => "ingress_pull_image",
        /// Replace the managed ingress gateway.
        IngressReplaceGateway => "ingress_replace_gateway",
        /// Start the managed Cloudflare tunnel connector.
        IngressStartTunnel => "ingress_start_tunnel",
        /// Start the managed tailnet node.
        IngressStartTailnet => "ingress_start_tailnet",
        /// Recover the managed ingress gateway.
        IngressRecoverGateway => "ingress_recover_gateway",
        /// Load routes into the managed ingress gateway.
        IngressConfigureRoutes => "ingress_configure_routes",
        /// Create DNS records for a site.
        IngressDnsCreate => "ingress_dns_create",
        /// Update DNS records of a site.
        IngressDnsUpdate => "ingress_dns_update",
        /// Delete DNS records of a site.
        IngressDnsDelete => "ingress_dns_delete",
        /// Issue a certificate through DNS-01.
        IngressCertificateIssue => "ingress_certificate_issue",
        /// Renew a certificate through DNS-01.
        IngressCertificateRenew => "ingress_certificate_renew",
        /// Delete an expired certificate.
        IngressCertificateDelete => "ingress_certificate_delete",
        /// Delete a leftover ACME challenge record.
        IngressCertificateChallengeDelete => "ingress_certificate_challenge_delete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Attempt `attempt` of one deployment, in `state`.
    fn attempt(attempt: u64, state: OperationState) -> Operation {
        Operation {
            id: "operation-1".into(),
            environment_id: EnvironmentId::parse("env-web-01").unwrap(),
            kind: OperationKind::Apply,
            generation: 1,
            attempt,
            consecutive_failures: 0,
            state,
            phase: None,
            resource: None,
            error_code: None,
            error_message: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            started_at_ms: None,
            finished_at_ms: None,
        }
    }

    /// The listed attempts' numbers and states.
    fn listed(attempts: &[Operation]) -> Vec<(u64, OperationState)> {
        attempts
            .iter()
            .map(|item| (item.attempt, item.state))
            .collect()
    }

    #[test]
    fn the_running_attempt_leads_until_its_outcome_is_retained() {
        let retained = [
            attempt(2, OperationState::Failed),
            attempt(1, OperationState::Failed),
        ];
        let running = attempt(3, OperationState::Running);
        assert_eq!(
            listed(&running.attempts(&retained)),
            [
                (3, OperationState::Running),
                (2, OperationState::Failed),
                (1, OperationState::Failed)
            ]
        );
        // A retained outcome is listed once, and nothing ran before the
        // first attempt.
        let ended = [attempt(3, OperationState::Failed)];
        assert_eq!(
            listed(&running.attempts(&ended)),
            [(3, OperationState::Failed)]
        );
        assert_eq!(
            listed(&attempt(0, OperationState::Requested).attempts(&[])),
            []
        );
    }

    #[test]
    fn a_failure_reads_as_its_code_and_message() {
        let failed = |code: Option<&str>, message: Option<&str>| Operation {
            error_code: code.map(str::to_owned),
            error_message: message.map(str::to_owned),
            ..attempt(1, OperationState::Failed)
        };
        assert_eq!(
            failed(Some("runtime_failed"), Some("it crashed")).failure(),
            "runtime_failed: it crashed"
        );
        assert_eq!(
            failed(Some("runtime_failed"), None).failure(),
            "runtime_failed"
        );
        assert_eq!(failed(None, Some("it crashed")).failure(), "it crashed");
        assert_eq!(failed(None, None).failure(), "");
    }
}
