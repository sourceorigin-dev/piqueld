//! An operation's progress as one ordered list of steps: what it did, what it
//! is doing, and what is left.
//!
//! The event journal is the only record. A [`Timeline`] is never stored: the
//! daemon folds an operation's events into one with [`Timeline::apply`], and
//! keeps applying new events to stream it live. Steps not yet started come
//! from the operation's latest recorded plan ([`EventDetail::Planned`]), or,
//! before the operation planned that far, from its environment's previous
//! successful operation ([`StepState::Forecast`]).

use crate::observability::{Diagnostic, Elapsed};
use crate::resource::TaskState;
use crate::{
    Event, EventDetail, EventKind, Operation, OperationKind, OperationState, Phase,
    names::string_enum,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

/// What a step does and on what: a journaled action's identity.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, ToSchema)]
pub struct StepKey {
    /// What the step does.
    pub phase: Phase,
    /// The logical or Docker resource it acts on, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// The step as people read it: its phase's label, then its resource, such
/// as "Build web".
impl std::fmt::Display for StepKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.phase.label())?;
        match &self.resource {
            Some(resource) => write!(formatter, " {resource}"),
            None => Ok(()),
        }
    }
}

string_enum! {
    #[serde]
    /// How far an operation has stepped through its work, in the order the
    /// steps of a deployment run.
    pub enum Stage {
        /// Fetching the manifest, building and resolving sources, planning.
        Prepare => "prepare",
        /// Before-rollout jobs and the cleanup of earlier runs.
        Jobs => "jobs",
        /// Creating, updating, waiting for and removing runtime resources.
        Rollout => "rollout",
        /// Shared infrastructure outside any one environment.
        Daemon => "daemon",
    }
}

impl Stage {
    /// The stage as a heading, such as "Roll out".
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Prepare => "Prepare",
            Self::Jobs => "Jobs",
            Self::Rollout => "Roll out",
            Self::Daemon => "Daemon",
        }
    }
}

impl Phase {
    /// What a step of this phase does, as people read it: "Build" for
    /// `building_git`. Followed by the step's resource, it names the step.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Prepare",
            Self::FetchManifest => "Fetch manifest",
            Self::BuildGit => "Build",
            Self::ResolveImage => "Resolve image",
            Self::Planning => "Plan",
            Self::RunJob => "Run job",
            Self::RemoveJobs => "Remove earlier jobs",
            Self::EnsureNetwork => "Ensure network",
            Self::EnsureVolume => "Ensure volume",
            Self::EnsureService => "Update service",
            Self::WaitForService => "Wait for service",
            Self::WaitForServiceRemoval => "Wait for service removal",
            Self::RemoveService => "Remove service",
            Self::RemoveNetwork => "Remove network",
            Self::RetainVolume => "Keep volume",
            Self::DetachNetwork => "Detach network",
            Self::RemoveVolumes => "Remove volumes",
            Self::RemoveSecrets => "Remove secrets",
            Self::Routing => "Apply routes",
            Self::Observation => "Observe",
            Self::EnsureSwarm => "Ensure Swarm",
            Self::RemoveImage => "Remove image",
            Self::DeleteRegistryImage => "Delete registry image",
            Self::IngressCreateNetwork => "Create ingress network",
            Self::IngressRemoveNetwork => "Remove ingress network",
            Self::IngressStartGateway => "Start gateway",
            Self::IngressStopGateway => "Stop gateway",
            Self::IngressRemoveContainer => "Remove ingress container",
            Self::IngressPullImage => "Pull gateway image",
            Self::IngressReplaceGateway => "Replace gateway",
            Self::IngressStartTunnel => "Start tunnel",
            Self::IngressStartTailnet => "Start tailnet node",
            Self::IngressRecoverGateway => "Recover gateway",
            Self::IngressConfigureRoutes => "Configure gateway routes",
            Self::IngressDnsCreate => "Create DNS records",
            Self::IngressDnsUpdate => "Update DNS records",
            Self::IngressDnsDelete => "Delete DNS records",
            Self::IngressCertificateIssue => "Issue certificate",
            Self::IngressCertificateRenew => "Renew certificate",
            Self::IngressCertificateDelete => "Delete certificate",
            Self::IngressCertificateChallengeDelete => "Delete certificate challenge",
        }
    }

    /// The stage of an operation this phase belongs to.
    #[must_use]
    pub const fn stage(self) -> Stage {
        match self {
            Self::Preparing
            | Self::FetchManifest
            | Self::BuildGit
            | Self::ResolveImage
            | Self::Planning => Stage::Prepare,
            Self::RunJob | Self::RemoveJobs => Stage::Jobs,
            Self::EnsureNetwork
            | Self::EnsureVolume
            | Self::EnsureService
            | Self::WaitForService
            | Self::WaitForServiceRemoval
            | Self::RemoveService
            | Self::RemoveNetwork
            | Self::RetainVolume
            | Self::DetachNetwork
            | Self::RemoveVolumes
            | Self::RemoveSecrets
            | Self::Routing
            | Self::Observation => Stage::Rollout,
            Self::EnsureSwarm
            | Self::RemoveImage
            | Self::DeleteRegistryImage
            | Self::IngressCreateNetwork
            | Self::IngressRemoveNetwork
            | Self::IngressStartGateway
            | Self::IngressStopGateway
            | Self::IngressRemoveContainer
            | Self::IngressPullImage
            | Self::IngressReplaceGateway
            | Self::IngressStartTunnel
            | Self::IngressStartTailnet
            | Self::IngressRecoverGateway
            | Self::IngressConfigureRoutes
            | Self::IngressDnsCreate
            | Self::IngressDnsUpdate
            | Self::IngressDnsDelete
            | Self::IngressCertificateIssue
            | Self::IngressCertificateRenew
            | Self::IngressCertificateDelete
            | Self::IngressCertificateChallengeDelete => Stage::Daemon,
        }
    }

    /// Whether steps of this phase run concurrently with each other:
    /// sources are prepared several at a time.
    #[must_use]
    pub const fn concurrent(self) -> bool {
        matches!(self, Self::BuildGit | Self::ResolveImage)
    }
}

impl crate::manifest::ValidatedSource {
    /// The phase preparing this source is journaled as.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        if matches!(self, Self::Git { .. }) {
            Phase::BuildGit
        } else {
            Phase::ResolveImage
        }
    }
}

string_enum! {
    #[serde]
    /// Where one step stands.
    pub enum StepState {
        /// Expected from the environment's previous successful operation;
        /// the operation has not planned that far yet.
        Forecast => "forecast",
        /// In the operation's latest plan, not started yet.
        Planned => "planned",
        /// Running.
        Running => "running",
        /// Failed a request and waiting to retry it.
        Retrying => "retrying",
        /// Finished successfully.
        Succeeded => "succeeded",
        /// Failed.
        Failed => "failed",
        /// Interrupted before its outcome was recorded, e.g. by a restart.
        Interrupted => "interrupted",
    }
}

impl StepState {
    /// Whether the step has started, so it has a place in time.
    #[must_use]
    pub const fn started(self) -> bool {
        !matches!(self, Self::Forecast | Self::Planned)
    }

    /// Whether the step has ended.
    #[must_use]
    pub const fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }

    /// Whether the step started and has not ended: running or retrying.
    #[must_use]
    pub const fn running(self) -> bool {
        matches!(self, Self::Running | Self::Retrying)
    }
}

/// One step of an operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Step {
    /// What it does and on what.
    #[serde(flatten)]
    pub key: StepKey,
    /// Where it stands.
    pub state: StepState,
    /// The journaled action running it, once started; filter events by it
    /// for its full history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// The operation attempt that ran it, once started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// When it started, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// When it ended, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    /// Requests retried after a retryable failure.
    #[serde(default)]
    pub retries: u64,
    /// How long it usually takes: the median of the same step in the
    /// environment's recent successful operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_ms: Option<u64>,
    /// Why it failed, or why its latest request did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
    /// While it waits for a service to roll out: the service's Swarm tasks,
    /// old and new, as they moved through their states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskLane>,
    /// How long Swarm watches each new task once it runs, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_seconds: Option<u32>,
}

/// One Swarm task of a rolling-out service, as one observation saw it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RolloutTask {
    /// Docker task ID.
    pub id: String,
    /// The replica slot it fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// Whether it runs the service's new specification.
    pub current: bool,
    /// Its Docker state.
    pub state: TaskState,
    /// Its health check verdict, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy: Option<bool>,
}

/// One Swarm task's states over a rollout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TaskLane {
    /// The task as last observed.
    #[serde(flatten)]
    pub task: RolloutTask,
    /// Its states in order, each from when it was first observed.
    pub spans: Vec<TaskSpan>,
}

/// A task's state from when it was first observed in it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TaskSpan {
    /// The Docker state.
    pub state: TaskState,
    /// The health check verdict, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy: Option<bool>,
    /// Since when, in Unix milliseconds.
    pub from_ms: i64,
}

impl TaskSpan {
    /// Whether the task serves requests during this span.
    #[must_use]
    pub fn serving(self) -> bool {
        self.state == TaskState::Running && self.healthy != Some(false)
    }
}

impl TaskLane {
    /// Whether the task serves requests as last observed.
    #[must_use]
    pub fn serving(&self) -> bool {
        self.spans.last().is_some_and(|span| span.serving())
    }

    /// Whether the task served requests at `at_ms`, as observed then.
    #[must_use]
    pub fn serving_at(&self, at_ms: i64) -> bool {
        self.spans
            .iter()
            .rfind(|span| span.from_ms <= at_ms)
            .is_some_and(|span| span.serving())
    }

    /// When Swarm watched, or watches, this new task for failure: for
    /// `monitor_seconds` from when it first served. `None` for an old task
    /// or one that never served.
    #[must_use]
    pub fn monitor(&self, monitor_seconds: u32) -> Option<Window> {
        if !self.task.current || monitor_seconds == 0 {
            return None;
        }
        let from_ms = self.spans.iter().find(|span| span.serving())?.from_ms;
        Some(Window {
            from_ms,
            to_ms: from_ms + i64::from(monitor_seconds) * 1_000,
        })
    }
}

/// A stretch of time, in Unix milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    /// When it opens.
    pub from_ms: i64,
    /// When it closes.
    pub to_ms: i64,
}

impl Window {
    /// How long it is left open at `now_ms`, zero once closed.
    #[must_use]
    pub fn left_ms(self, now_ms: i64) -> u64 {
        u64::try_from(self.to_ms - now_ms).unwrap_or(0)
    }

    /// Whether it is open at `now_ms`.
    #[must_use]
    pub const fn contains(self, now_ms: i64) -> bool {
        self.from_ms <= now_ms && now_ms < self.to_ms
    }
}

/// How many of a rollout's new tasks serve requests, such as "1/2 new tasks
/// running".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NewTasks {
    /// New tasks serving.
    pub running: usize,
    /// New tasks not over: serving, or on their way to it.
    pub total: usize,
}

impl std::fmt::Display for NewTasks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { running, total } = self;
        let tasks = if *total == 1 { "task" } else { "tasks" };
        write!(formatter, "{running}/{total} new {tasks} running")
    }
}

/// When no task of a rolling-out service served requests.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Gap {
    /// When the last serving task stopped serving, in Unix milliseconds.
    pub from_ms: i64,
    /// When a task served again; `None` while none does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
}

impl Gap {
    /// When it ended, or `end_ms` while it has not.
    #[must_use]
    pub fn until(self, end_ms: i64) -> i64 {
        self.to_ms.unwrap_or(end_ms)
    }
}

/// How long no task of a rolling-out service served requests altogether.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Downtime {
    /// For how long, in milliseconds.
    pub ms: u64,
    /// Whether no task serves still.
    pub ongoing: bool,
}

impl Step {
    /// A step not started yet, expected to take `expected_ms`.
    #[must_use]
    pub const fn pending(key: StepKey, state: StepState, expected_ms: Option<u64>) -> Self {
        Self {
            key,
            state,
            action_id: None,
            attempt: None,
            started_at_ms: None,
            finished_at_ms: None,
            retries: 0,
            expected_ms,
            diagnostic: None,
            tasks: Vec::new(),
            monitor_seconds: None,
        }
    }

    /// Records an observation, at `at_ms`, of the service's tasks while
    /// this step waits for it: a span for each task whose state changed.
    fn observe(&mut self, tasks: &[RolloutTask], monitor_seconds: u32, at_ms: i64) {
        self.monitor_seconds = Some(monitor_seconds);
        for task in tasks {
            let span = TaskSpan {
                state: task.state,
                healthy: task.healthy,
                from_ms: at_ms,
            };
            match self.tasks.iter_mut().find(|lane| lane.task.id == task.id) {
                Some(lane) => {
                    let changed = lane.spans.last().is_none_or(|last| {
                        (last.state, last.healthy) != (span.state, span.healthy)
                    });
                    if changed {
                        lane.spans.push(span);
                    }
                    lane.task = task.clone();
                }
                None => self.tasks.push(TaskLane {
                    task: task.clone(),
                    spans: vec![span],
                }),
            }
        }
    }

    /// When no task of the service served requests while this step waited
    /// for it, from the first observation on; a stop-first rollout has one.
    #[must_use]
    pub fn gaps(&self) -> Vec<Gap> {
        let mut changes: Vec<(i64, bool)> = self
            .tasks
            .iter()
            .flat_map(|lane| {
                let mut serving = false;
                lane.spans.iter().filter_map(move |span| {
                    let now = span.serving();
                    (now != serving).then(|| {
                        serving = now;
                        (span.from_ms, now)
                    })
                })
            })
            .collect();
        changes.sort_by_key(|(at, starts)| (*at, !starts));
        let mut gaps = Vec::new();
        let mut serving = 0usize;
        let mut started = false;
        for (at, starts) in changes {
            if starts {
                if serving == 0
                    && let Some(gap) = gaps.last_mut().filter(|gap: &&mut Gap| gap.to_ms.is_none())
                {
                    gap.to_ms = Some(at);
                }
                serving += 1;
                started = true;
            } else {
                serving = serving.saturating_sub(1);
                if serving == 0 && started {
                    gaps.push(Gap {
                        from_ms: at,
                        to_ms: None,
                    });
                }
            }
        }
        gaps
    }

    /// How long no task of the service served requests, as of `now_ms` or
    /// the step's end; `None` when a task always did.
    #[must_use]
    pub fn downtime(&self, now_ms: i64) -> Option<Downtime> {
        let end = self.finished_at_ms.unwrap_or(now_ms);
        let gaps = self.gaps();
        let ms = gaps
            .iter()
            .map(|gap| u64::try_from(gap.until(end) - gap.from_ms).unwrap_or(0))
            .sum();
        let ongoing = gaps.last()?.to_ms.is_none();
        Some(Downtime { ms, ongoing })
    }

    /// Whether an old task was seen still serving once a new one served,
    /// with no gap since: requests always found a task, as in a
    /// `start-first` rollout. A handover between two observations shows
    /// neither this nor a gap.
    #[must_use]
    pub fn overlapped(&self) -> bool {
        let (new, old): (Vec<_>, Vec<_>) = self.tasks.iter().partition(|lane| lane.task.current);
        self.gaps().is_empty()
            && new
                .iter()
                .filter_map(|lane| lane.spans.iter().find(|span| span.serving()))
                .any(|span| old.iter().any(|lane| lane.serving_at(span.from_ms)))
    }

    /// How many of the rollout's new tasks serve; `None` before any was
    /// observed.
    #[must_use]
    pub fn new_tasks(&self) -> Option<NewTasks> {
        let new = self.tasks.iter().filter(|lane| lane.task.current);
        let tasks = NewTasks {
            running: new.clone().filter(|lane| lane.serving()).count(),
            total: new.filter(|lane| !lane.task.state.ended()).count(),
        };
        (tasks.total > 0).then_some(tasks)
    }

    /// The monitor window open at `now_ms` while the step runs: Swarm
    /// watches a new task for failure before it replaces the next one.
    #[must_use]
    pub fn monitor(&self, now_ms: i64) -> Option<Window> {
        let seconds = self.monitor_seconds.filter(|_| self.state.running())?;
        self.tasks
            .iter()
            .filter_map(|lane| lane.monitor(seconds))
            .filter(|window| window.contains(now_ms))
            .max_by_key(|window| window.to_ms)
    }

    /// How much longer it should take at `now_ms`: its expected duration
    /// less the time it has run, at least zero; zero once it ended, and
    /// `None` without an expectation.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: i64) -> Option<u64> {
        if self.state.finished() {
            return Some(0);
        }
        let expected = self.expected_ms?;
        Some(expected.saturating_sub(self.duration_ms(now_ms).unwrap_or(0)))
    }

    /// How long it ran, or has been running at `now_ms`; `None` before it
    /// started.
    #[must_use]
    pub fn duration_ms(&self, now_ms: i64) -> Option<u64> {
        let started = self.started_at_ms?;
        let ended = self.finished_at_ms.unwrap_or(now_ms);
        Some(u64::try_from(ended - started).unwrap_or(0))
    }

    /// How long it usually takes, when it succeeded in noticeably more or
    /// less time than that.
    #[must_use]
    pub fn unusual_ms(&self) -> Option<u64> {
        if self.state != StepState::Succeeded {
            return None;
        }
        let expected = self.expected_ms?;
        let took = self.duration_ms(self.finished_at_ms?)?;
        Self::noticeable(took, expected).then_some(expected)
    }

    /// Whether taking `took_ms` differs noticeably from the usual
    /// `expected_ms`: by half of it, and a second at least.
    #[must_use]
    pub fn noticeable(took_ms: u64, expected_ms: u64) -> bool {
        took_ms.abs_diff(expected_ms) >= (expected_ms / 2).max(1_000)
    }
}

/// How long a step usually takes, from an environment's recent successful
/// operations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ExpectedStep {
    /// The step.
    #[serde(flatten)]
    pub key: StepKey,
    /// Its median duration in milliseconds.
    pub duration_ms: u64,
}

/// An operation's steps, folded from its events.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Timeline {
    /// The operation, as of the newest event applied.
    pub operation: Operation,
    /// Its steps: those started, in the order they started, then those left,
    /// in the order they are expected to run.
    pub steps: Vec<Step>,
    /// When the operation's target was promoted, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at_ms: Option<i64>,
    /// The newest event applied: the stream resumes after it.
    pub cursor: i64,
    /// Expected durations, which plans and started steps look up.
    #[serde(skip)]
    expected: Vec<ExpectedStep>,
}

impl Timeline {
    /// An operation's timeline before any of its events: `forecast`, the
    /// steps of the environment's previous successful operation in their
    /// order, with how long each step usually takes from `expected`.
    #[must_use]
    pub fn new(operation: Operation, forecast: Vec<StepKey>, expected: Vec<ExpectedStep>) -> Self {
        let mut timeline = Self {
            operation,
            steps: Vec::new(),
            promoted_at_ms: None,
            cursor: 0,
            expected,
        };
        timeline.steps = forecast
            .into_iter()
            .map(|key| {
                let expected = timeline.expected_ms(&key);
                Step::pending(key, StepState::Forecast, expected)
            })
            .collect();
        timeline.retain_steps();
        timeline
    }

    /// Applies one of the operation's events, newer than [`Self::cursor`];
    /// older events, and those of other operations, are ignored.
    pub fn apply(&mut self, event: &Event) {
        if event.id <= self.cursor || event.operation_id.as_deref() != Some(&self.operation.id) {
            return;
        }
        self.cursor = event.id;
        if event
            .attempt
            .is_some_and(|attempt| !self.operation.current_attempt(attempt))
        {
            return;
        }
        if let Some(EventDetail::Planned { steps, coverage }) = &event.detail {
            self.plan(steps, *coverage);
            return;
        }
        let Some(kind) = event.known_kind() else {
            return;
        };
        match kind {
            EventKind::TargetPromoted => self.promoted_at_ms = Some(event.created_at_ms),
            EventKind::ActionStarted => self.start(event),
            kind => {
                let Some(step) = self.action(event) else {
                    return;
                };
                match kind {
                    EventKind::ActionRequested => {
                        step.retries = event.retry.unwrap_or(1).saturating_sub(1);
                        step.state = StepState::Running;
                    }
                    EventKind::ActionProgressed => {
                        if let Some(EventDetail::Rollout {
                            tasks,
                            monitor_seconds,
                        }) = &event.detail
                        {
                            step.observe(tasks, *monitor_seconds, event.created_at_ms);
                        }
                    }
                    EventKind::ActionRetry => {
                        step.state = StepState::Retrying;
                        step.diagnostic.clone_from(&event.diagnostic);
                    }
                    EventKind::ActionSucceeded
                    | EventKind::ActionFailed
                    | EventKind::ActionOutcomeUnknown => {
                        step.state = match kind {
                            EventKind::ActionSucceeded => StepState::Succeeded,
                            EventKind::ActionFailed => StepState::Failed,
                            _ => StepState::Interrupted,
                        };
                        step.finished_at_ms = Some(event.created_at_ms);
                        step.diagnostic.clone_from(&event.diagnostic);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Updates the operation as read after its events changed it. Once it is
    /// requested or runs again, the steps earlier attempts started are
    /// dropped, and once it ended, its forecast.
    pub fn set_operation(&mut self, operation: Operation) {
        self.operation = operation;
        self.retain_steps();
    }

    /// Keeps the steps the timeline shows: those of the current attempt,
    /// and forecast ones only while the operation may still run them.
    fn retain_steps(&mut self) {
        let operation = &self.operation;
        self.steps.retain(|step| {
            step.attempt
                .is_none_or(|attempt| operation.current_attempt(attempt))
                && !(step.state == StepState::Forecast && operation.state.terminal())
        });
    }

    /// Whether the operation ended and none of its steps still runs, so the
    /// timeline stays as it is unless the operation is requested again.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.operation.state.terminal() && !self.steps.iter().any(|step| step.state.running())
    }

    /// How much longer the operation should take at `now_ms`: the steps left
    /// run one after another, except sources, which are prepared at once.
    /// `None` once it ended, or when no step left has an expectation.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: i64) -> Option<u64> {
        if self.operation.state.terminal() {
            return None;
        }
        Self::chain(
            self.steps
                .iter()
                .filter(|step| !step.state.finished())
                .map(|step| (step.key.phase, step.remaining_ms(now_ms))),
        )
    }

    /// How long the operation's steps usually take altogether, run as
    /// [`Self::remaining_ms`] runs them. `None` when no step has an
    /// expectation.
    #[must_use]
    pub fn usual_ms(&self) -> Option<u64> {
        Self::chain(
            self.steps
                .iter()
                .map(|step| (step.key.phase, step.expected_ms)),
        )
    }

    /// [`Self::usual_ms`], to compare with how long the operation took: only
    /// once it and every step succeeded, each with an expectation, since a
    /// failed or stopped run's steps are not the usual ones, and a step new
    /// to it would count only in how long it took.
    #[must_use]
    pub fn comparable_usual_ms(&self) -> Option<u64> {
        (self.operation.state == OperationState::Succeeded
            && self
                .steps
                .iter()
                .all(|step| step.state == StepState::Succeeded && step.expected_ms.is_some()))
        .then(|| self.usual_ms())
        .flatten()
    }

    /// How long steps of the given phases and durations take: one after
    /// another, except sources, which are prepared at once. Steps without a
    /// duration are skipped; `None` when none has one.
    fn chain(steps: impl Iterator<Item = (Phase, Option<u64>)>) -> Option<u64> {
        let mut known = false;
        let mut sequential = 0u64;
        let mut concurrent = 0u64;
        for (phase, duration) in steps {
            let Some(duration) = duration else {
                continue;
            };
            known = true;
            if phase.concurrent() {
                concurrent = concurrent.max(duration);
            } else {
                sequential += duration;
            }
        }
        known.then_some(sequential + concurrent)
    }

    /// The step an event of a started action belongs to.
    fn action(&mut self, event: &Event) -> Option<&mut Step> {
        let action = event.action_id.as_deref()?;
        self.steps
            .iter_mut()
            .rev()
            .find(|step| step.action_id.as_deref() == Some(action))
    }

    /// Starts the step `event` names: the first one left with its key, or a
    /// new one after those started.
    fn start(&mut self, event: &Event) {
        let Some(phase) = event.phase else {
            return;
        };
        let key = StepKey {
            phase,
            resource: event.resource.clone(),
        };
        let index = self
            .steps
            .iter()
            .position(|step| !step.state.started() && step.key == key)
            .unwrap_or_else(|| {
                let expected = self.expected_ms(&key);
                self.steps
                    .push(Step::pending(key, StepState::Planned, expected));
                self.steps.len() - 1
            });
        let step = &mut self.steps[index];
        step.state = StepState::Running;
        step.action_id.clone_from(&event.action_id);
        step.attempt = event.attempt;
        step.started_at_ms = Some(event.created_at_ms);
        self.order();
    }

    /// Replaces the steps left that `coverage` plans with `planned`.
    fn plan(&mut self, planned: &[StepKey], coverage: PlanCoverage) {
        self.steps
            .retain(|step| step.state.started() || !coverage.covers(step.key.phase.stage()));
        for key in planned {
            let running = self
                .steps
                .iter()
                .any(|step| step.key == *key && step.state.running());
            if !running {
                let expected = self.expected_ms(key);
                self.steps
                    .push(Step::pending(key.clone(), StepState::Planned, expected));
            }
        }
        self.order();
    }

    /// Keeps the started steps first, in the order they started, then the
    /// steps left by stage, each stage's in the order it was planned.
    fn order(&mut self) {
        self.steps.sort_by_key(|step| {
            if step.state.started() {
                (false, None)
            } else {
                (true, Some(step.key.phase.stage() as u8))
            }
        });
    }

    /// How long `key` usually takes.
    fn expected_ms(&self, key: &StepKey) -> Option<u64> {
        self.expected
            .iter()
            .find(|step| step.key == *key)
            .map(|step| step.duration_ms)
    }
}

string_enum! {
    #[serde]
    /// Which of an operation's steps a recorded plan lists.
    pub enum PlanCoverage {
        /// Preparing sources, before the runtime is planned.
        Preparation => "preparation",
        /// Running jobs, once sources are prepared.
        Jobs => "jobs",
        /// Everything left: preparation is done by the time the runtime is
        /// planned.
        Runtime => "runtime",
    }
}

impl PlanCoverage {
    /// The phase an operation is in while recording a plan of this coverage.
    #[must_use]
    pub const fn phase(self) -> Phase {
        match self {
            Self::Preparation | Self::Jobs => Phase::Preparing,
            Self::Runtime => Phase::Planning,
        }
    }

    /// Whether a plan of this coverage replaces the steps left of `stage`.
    const fn covers(self, stage: Stage) -> bool {
        match self {
            Self::Preparation => matches!(stage, Stage::Prepare),
            Self::Jobs => matches!(stage, Stage::Jobs),
            Self::Runtime => !matches!(stage, Stage::Daemon),
        }
    }
}

/// How far an operation has come, as `piquelctl`'s live header and the
/// dashboard's progress show it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    /// Kind of operation it tracks.
    kind: OperationKind,
    /// Where the operation stands.
    pub state: OperationState,
    /// Steps finished.
    pub done: usize,
    /// Steps altogether, done and left.
    pub total: usize,
    /// The steps running, such as "Build web".
    pub running: Vec<String>,
    /// How long it has run, or ran, from the first start of the operation
    /// or any step; `None` before anything started.
    pub elapsed_ms: Option<u64>,
    /// How much longer it should take, while it runs.
    pub left_ms: Option<u64>,
    /// How long it usually takes, while it runs; once it ended, only when
    /// its run compares with the usual one.
    pub usual_ms: Option<u64>,
}

impl Progress {
    /// Where `timeline` stands at `now_ms`.
    #[must_use]
    pub fn new(timeline: &Timeline, now_ms: i64) -> Self {
        let operation = &timeline.operation;
        let started = timeline
            .steps
            .iter()
            .filter_map(|step| step.started_at_ms)
            .chain(operation.started_at_ms)
            .min();
        let end = operation.finished_at_ms.unwrap_or(now_ms);
        Self {
            kind: operation.kind,
            state: operation.state,
            done: timeline
                .steps
                .iter()
                .filter(|step| step.state.finished())
                .count(),
            total: timeline.steps.len(),
            running: timeline
                .steps
                .iter()
                .filter(|step| step.state.running())
                .map(|step| step.key.to_string())
                .collect(),
            elapsed_ms: started
                .map(|started| u64::try_from(end.saturating_sub(started)).unwrap_or(0)),
            left_ms: timeline.remaining_ms(now_ms),
            usual_ms: if operation.state.terminal() {
                timeline.comparable_usual_ms()
            } else {
                timeline.usual_ms()
            },
        }
    }

    /// How many of `cells` the steps done fill, rounded down: a bar of
    /// `cells` cells, or with 100, the percent done.
    #[must_use]
    pub fn filled(&self, cells: usize) -> usize {
        (self.done * cells).checked_div(self.total).unwrap_or(0)
    }

    /// Steps done, in percent, rounded down.
    #[must_use]
    pub fn percent(&self) -> usize {
        self.filled(100)
    }

    /// Where the operation stands, in a word: "Deploying" or "Deployed".
    #[must_use]
    pub const fn status(&self) -> &'static str {
        let delete = matches!(self.kind, OperationKind::Delete);
        match self.state {
            OperationState::Requested => "Queued",
            OperationState::Running if delete => "Deleting",
            OperationState::Running => "Deploying",
            OperationState::Succeeded if delete => "Deleted",
            OperationState::Succeeded => "Deployed",
            OperationState::Failed => "Failed",
            OperationState::Cancelled => "Cancelled",
            OperationState::Superseded => "Superseded",
        }
    }

    /// Steps done of all, such as "4/9".
    #[must_use]
    pub fn steps(&self) -> String {
        format!("{}/{}", self.done, self.total)
    }

    /// How long a running operation should still take, such as "~48.0 s
    /// left", or how a finished one compared with its usual duration.
    #[must_use]
    pub fn outlook(&self) -> Option<String> {
        self.against_usual().or_else(|| {
            let left = self.left_ms.filter(|_| !self.state.terminal())?;
            Some(format!("~{} left", Elapsed(Duration::from_millis(left))))
        })
    }

    /// How a finished operation compared with its usual duration, such as
    /// "6.0 s faster than usual".
    fn against_usual(&self) -> Option<String> {
        let usual = self.usual_ms.filter(|_| self.state.terminal())?;
        let elapsed = self.elapsed_ms?;
        let difference = elapsed.abs_diff(usual);
        Some(if difference < 1_000 {
            "as long as usual".to_owned()
        } else {
            let faster = if elapsed < usual { "faster" } else { "slower" };
            format!(
                "{} {faster} than usual",
                Elapsed(Duration::from_millis(difference))
            )
        })
    }
}

#[cfg(test)]
mod tests;
