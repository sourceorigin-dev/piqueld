//! Informational control-plane history; never used to reconstruct engine state.
use crate::names::{compared_as_string, string_enum};
use crate::{ApplicationId, EnvironmentId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One immutable, sanitized fact about control-plane activity.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct Event {
    /// Monotonic event identity used for pagination.
    pub id: i64,
    /// Related application, including for events of its deleted environments.
    #[serde(default)]
    pub application_id: Option<ApplicationId>,
    /// Related environment; absent for application-wide events such as edits.
    pub environment_id: Option<EnvironmentId>,
    /// Related operation, retained even if that operation is pruned.
    pub operation_id: Option<String>,
    /// Intent generation, when applicable.
    pub generation: Option<u64>,
    /// Execution attempt for attempt-related events.
    pub attempt: Option<u64>,
    /// Stable event category, such as `operation_failed` or `health_changed`.
    pub kind: String,
    /// Sanitized diagnostic or state description.
    pub message: Option<String>,
    /// Stable failure classification, when this event records failure.
    pub error_code: Option<String>,
    /// Execution phase at the time of the event.
    pub phase: Option<crate::Phase>,
    /// Related logical or Docker resource.
    pub resource: Option<String>,
    /// Lifetime owner, independent of contextual environment references.
    #[serde(default)]
    pub scope: crate::observability::EventScope,
    /// Action execution identity.
    #[serde(default)]
    pub action_id: Option<String>,
    /// One-based action request attempt.
    #[serde(default)]
    pub retry: Option<u64>,
    /// Scheduled delay before another request, in milliseconds.
    #[serde(default)]
    pub retry_delay_ms: Option<u64>,
    /// Action duration in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Correlated API request identity.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Who caused this event, including runtime actions of the operation it
    /// requested; absent for the daemon's own work.
    #[serde(default)]
    pub actor: Option<EventActor>,
    /// Safe, independently readable failure details.
    #[serde(default)]
    pub diagnostic: Option<crate::observability::Diagnostic>,
    /// Structured facts specific to the event's kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<EventDetail>,
    /// Unix timestamp in milliseconds.
    pub created_at_ms: i64,
}

impl Event {
    /// The event's kind, when this build knows it.
    #[must_use]
    pub fn known_kind(&self) -> Option<EventKind> {
        EventKind::parse(&self.kind)
    }
}

string_enum! {
    /// Every kind of event the daemon records, as stored in history and sent
    /// as [`Event::kind`]. A kind never changes, but new ones are added.
    pub enum EventKind {
        /// An apply operation was requested, or applying a fetched manifest
        /// changed the application.
        ApplicationApplied => "application_applied",
        /// A refresh operation was requested.
        RefreshRequested => "refresh_requested",
        /// A delete operation was requested.
        DeletionRequested => "deletion_requested",
        /// An operation was requested again, to be retried.
        ReconciliationRequested => "reconciliation_requested",
        /// A rollback to an earlier deployment was requested.
        RollbackRequested => "rollback_requested",
        /// A failed rollout could not be rolled back automatically; the
        /// message says why.
        RollbackUnavailable => "rollback_unavailable",
        /// An operation started running.
        OperationStarted => "operation_started",
        /// An operation succeeded.
        OperationSucceeded => "operation_succeeded",
        /// An operation failed, or a deletion failed an attempt it retries.
        OperationFailed => "operation_failed",
        /// An operation was cancelled.
        OperationCancelled => "operation_cancelled",
        /// A newer request replaced a requested or running operation.
        OperationSuperseded => "operation_superseded",
        /// The daemon restarted while an operation ran; it runs again.
        OperationInterrupted => "operation_interrupted",
        /// An operation fetched its manifest; the message is the commit.
        ManifestFetched => "manifest_fetched",
        /// An operation resolved the release it deploys.
        TargetResolved => "target_resolved",
        /// An operation published its resolved release to its environment.
        TargetPromoted => "target_promoted",
        /// A tracked branch moved, starting a deployment of its new head.
        BranchSynced => "branch_synced",
        /// An operation's environment changed state; the message is the new
        /// state.
        StatusChanged => "status_changed",
        /// The runtime health of an operation's environment changed; the
        /// message is the new health.
        HealthChanged => "health_changed",
        /// An action of an operation changed runtime state.
        ResourceMutated => "resource_mutated",
        /// An operation planned the steps it runs next, listed in its
        /// [`EventDetail::Planned`].
        PlanRecorded => "plan_recorded",

        /// An action was journaled, before it is requested.
        ActionStarted => "action_started",
        /// An action's request is about to be sent, as its `retry`th attempt.
        ActionRequested => "action_requested",
        /// An action's attempt failed and will be retried.
        ActionRetry => "action_retry",
        /// A running action observed progress, such as a service's rollout,
        /// listed in its [`EventDetail::Rollout`].
        ActionProgressed => "action_progressed",
        /// An action succeeded.
        ActionSucceeded => "action_succeeded",
        /// An action failed.
        ActionFailed => "action_failed",
        /// An action was still open when the daemon stopped, so whether it
        /// took effect is unknown.
        ActionOutcomeUnknown => "action_outcome_unknown",

        /// An application's configuration was edited, as its
        /// [`EventDetail::Edited`] says.
        ApplicationEdited => "application_edited",
        /// An application was renamed.
        ApplicationRenamed => "application_renamed",
        /// An application's webhook secret was replaced.
        WebhookSecretGenerated => "webhook_secret_generated",
        /// An environment was created.
        EnvironmentCreated => "environment_created",
        /// An environment was renamed.
        EnvironmentRenamed => "environment_renamed",
        /// An environment's manifest source changed.
        EnvironmentSourceChanged => "environment_source_changed",
        /// An environment's branch tracking changed.
        EnvironmentSyncChanged => "environment_sync_changed",
        /// An environment was granted, or lost, every application's
        /// telemetry.
        EnvironmentTelemetryChanged => "environment_telemetry_changed",
        /// An environment was stopped.
        EnvironmentStopped => "environment_stopped",
        /// A stopped environment was started.
        EnvironmentStarted => "environment_started",
        /// A preview environment was created.
        PreviewCreated => "preview_created",
        /// A command started in one of an environment's services.
        CommandStarted => "command_started",
        /// A command run in an environment's service finished.
        CommandFinished => "command_finished",

        /// A secret's value was stored.
        SecretSaved => "secret_saved",
        /// A secret was deleted.
        SecretDeleted => "secret_deleted",
        /// Which environments may use a stored secret changed.
        SecretAccessChanged => "secret_access_changed",
        /// Secret key recovery discarded the stored values of an application
        /// or environment.
        SecretValuesDiscarded => "secret_values_discarded",

        /// An unexpected failure outside any operation, with its diagnostic.
        Diagnostic => "diagnostic",
        /// A dependency of the daemon, such as certificate renewal or
        /// telemetry export, failed or recovered.
        DependencyHealthChanged => "dependency_health_changed",
        /// The daemon started; the resource is its version.
        DaemonStarted => "daemon_started",
        /// The daemon's state was rolled back to a backup, because an older
        /// version started on a database a newer one had migrated; the
        /// resource is the backup.
        DaemonRolledBack => "daemon_rolled_back",

        /// An account received `admin` on every application.
        AdminGranted => "admin_granted",
        /// A token was created without expiry or with `admin`.
        PrivilegedTokenCreated => "privileged_token_created",
        /// Many requests from one address and account were refused within a
        /// minute; the resource is that address.
        AccessDenialBurst => "access_denial_burst",
        /// An admin recovery link was issued over the Unix socket.
        AdminRecoveryIssued => "admin_recovery_issued",
        /// A host operator sign-in link was issued over the Unix socket.
        OperatorSignInIssued => "operator_sign_in_issued",
        /// A credential was used from an address it had not used before.
        CredentialNewAddress => "credential_new_address",
        /// The secret master key was recovered, discarding stored values.
        SecretKeyRecovered => "secret_key_recovered",
        /// A notification destination was created, changed or deleted.
        NotificationDestinationChanged => "notification_destination_changed",
    }
}

compared_as_string!(EventKind);

/// Structured facts an event records beyond its common fields, tagged by
/// `type`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventDetail {
    /// A typed configuration edit (`application_edited`).
    Edited {
        /// The part of the configuration it changed.
        field: crate::edit::EditedField,
    },
    /// The steps an operation plans to run next (`plan_recorded`), replacing
    /// those it planned before for the same `coverage`.
    Planned {
        /// The steps, in the order they are planned to run.
        steps: Vec<crate::timeline::StepKey>,
        /// Which of the operation's steps the plan lists.
        coverage: crate::timeline::PlanCoverage,
    },
    /// What a service's Swarm tasks are doing while an action waits for it
    /// to roll out (`action_progressed`), recorded when that changes.
    Rollout {
        /// The tasks of this rollout: running and starting ones, and those
        /// it stops.
        tasks: Vec<crate::timeline::RolloutTask>,
        /// How long Swarm watches each new task once it runs, in seconds.
        monitor_seconds: u32,
    },
}

/// Who caused an event: exactly one kind of actor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventActor {
    /// An account's API request.
    Account {
        /// The account.
        user_id: String,
        /// The credential that authenticated the request, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_id: Option<String>,
    },
    /// The host operator, instead of an account.
    Operator {
        /// Its Unix user.
        operator: crate::auth::HostOperator,
        /// The browser session it acted through, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// An automated actor of the daemon, e.g. `sync:poll`.
    System {
        /// Which one.
        actor: crate::sync::SystemActor,
    },
}

impl EventActor {
    /// The credential or host operator session the request used, which the
    /// audit trail is filtered by.
    #[must_use]
    pub fn credential_id(&self) -> Option<&str> {
        match self {
            Self::Account { credential_id, .. } => credential_id.as_deref(),
            Self::Operator { session_id, .. } => session_id.as_deref(),
            Self::System { .. } => None,
        }
    }
}

/// ```text
/// user-01…    host operator (uid 0)    sync:poll
/// ```
impl std::fmt::Display for EventActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Account { user_id, .. } => formatter.write_str(user_id),
            Self::Operator { operator, .. } => operator.fmt(formatter),
            Self::System { actor } => actor.fmt(formatter),
        }
    }
}
