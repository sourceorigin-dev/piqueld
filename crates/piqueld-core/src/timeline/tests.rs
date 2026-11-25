use super::*;
use crate::{EnvironmentId, OperationKind};

fn key(phase: Phase, resource: &str) -> StepKey {
    StepKey {
        phase,
        resource: Some(resource.into()),
    }
}

fn timeline(expected: &[(Phase, &str, u64)]) -> Timeline {
    let operation = Operation {
        id: "op".into(),
        environment_id: EnvironmentId::parse("env-web-01").unwrap(),
        kind: OperationKind::Refresh,
        generation: 1,
        attempt: 1,
        consecutive_failures: 0,
        state: OperationState::Running,
        phase: None,
        resource: None,
        error_code: None,
        error_message: None,
        created_at_ms: 0,
        updated_at_ms: 0,
        started_at_ms: Some(0),
        finished_at_ms: None,
    };
    let expected: Vec<_> = expected
        .iter()
        .map(|(phase, resource, duration_ms)| ExpectedStep {
            key: key(*phase, resource),
            duration_ms: *duration_ms,
        })
        .collect();
    let forecast = expected.iter().map(|step| step.key.clone()).collect();
    Timeline::new(operation, forecast, expected)
}

/// Applies events numbered from the timeline's cursor.
fn apply(timeline: &mut Timeline, events: Vec<Event>) {
    for (offset, mut event) in events.into_iter().enumerate() {
        event.id = timeline.cursor + 1 + i64::try_from(offset).unwrap();
        event.operation_id = Some("op".into());
        event.attempt = Some(1);
        timeline.apply(&event);
    }
}

fn action(kind: &str, id: &str, phase: Phase, resource: &str, at: i64) -> Event {
    Event {
        kind: kind.into(),
        action_id: Some(id.into()),
        phase: Some(phase),
        resource: Some(resource.into()),
        created_at_ms: at,
        ..Event::default()
    }
}

fn planned(coverage: PlanCoverage, steps: &[StepKey]) -> Event {
    Event {
        kind: "plan_recorded".into(),
        detail: Some(EventDetail::Planned {
            steps: steps.to_vec(),
            coverage,
        }),
        ..Event::default()
    }
}

fn states(timeline: &Timeline) -> Vec<(Phase, StepState)> {
    timeline
        .steps
        .iter()
        .map(|step| (step.key.phase, step.state))
        .collect()
}

#[test]
fn plans_replace_the_forecast_and_actions_fill_them_in() {
    let mut timeline = timeline(&[
        (Phase::BuildGit, "web", 30_000),
        (Phase::RunJob, "migrate", 4_000),
        (Phase::EnsureService, "web", 2_000),
        (Phase::WaitForService, "web", 10_000),
    ]);
    assert_eq!(
        states(&timeline),
        [
            (Phase::BuildGit, StepState::Forecast),
            (Phase::RunJob, StepState::Forecast),
            (Phase::EnsureService, StepState::Forecast),
            (Phase::WaitForService, StepState::Forecast),
        ]
    );
    // Each plan replaces only its own stage's forecast.
    apply(
        &mut timeline,
        vec![planned(
            PlanCoverage::Preparation,
            &[key(Phase::BuildGit, "web")],
        )],
    );
    assert_eq!(
        states(&timeline),
        [
            (Phase::BuildGit, StepState::Planned),
            (Phase::RunJob, StepState::Forecast),
            (Phase::EnsureService, StepState::Forecast),
            (Phase::WaitForService, StepState::Forecast),
        ]
    );
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "web", 100),
            action("action_succeeded", "a1", Phase::BuildGit, "web", 25_100),
            planned(
                PlanCoverage::Runtime,
                &[
                    key(Phase::EnsureVolume, "data"),
                    key(Phase::EnsureService, "web"),
                ],
            ),
            action("action_started", "a2", Phase::EnsureVolume, "data", 25_200),
        ],
    );
    assert_eq!(
        states(&timeline),
        [
            (Phase::BuildGit, StepState::Succeeded),
            (Phase::EnsureVolume, StepState::Running),
            (Phase::EnsureService, StepState::Planned),
        ]
    );
    let build = &timeline.steps[0];
    assert_eq!(
        (build.started_at_ms, build.finished_at_ms, build.expected_ms),
        (Some(100), Some(25_100), Some(30_000))
    );
    assert_eq!(timeline.steps[2].expected_ms, Some(2_000));
}

#[test]
fn an_operation_that_ended_forecasts_nothing() {
    let mut timeline = timeline(&[
        (Phase::BuildGit, "web", 30_000),
        (Phase::EnsureService, "web", 2_000),
    ]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "web", 0),
            action("action_failed", "a1", Phase::BuildGit, "web", 100),
        ],
    );
    let mut failed = timeline.operation.clone();
    failed.state = OperationState::Failed;
    // Followed live, as a new read does.
    timeline.set_operation(failed.clone());
    assert_eq!(states(&timeline), [(Phase::BuildGit, StepState::Failed)]);
    let read = Timeline::new(failed, vec![key(Phase::EnsureService, "web")], Vec::new());
    assert_eq!(states(&read), Vec::new());
}

#[test]
fn retries_and_failures_stay_on_their_step() {
    let mut timeline = timeline(&[]);
    let mut retry = action("action_retry", "a1", Phase::EnsureService, "web", 200);
    retry.diagnostic = Some(Diagnostic::from_recorded_code(
        "d1".into(),
        "docker_unavailable",
        "Docker is unavailable".into(),
    ));
    let mut requested = action("action_requested", "a1", Phase::EnsureService, "web", 300);
    requested.retry = Some(3);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::EnsureService, "web", 100),
            retry,
        ],
    );
    assert_eq!(timeline.steps[0].state, StepState::Retrying);
    apply(
        &mut timeline,
        vec![
            requested,
            action("action_failed", "a1", Phase::EnsureService, "web", 400),
            // A new attempt runs the same step again as its own.
            action("action_started", "a2", Phase::EnsureService, "web", 500),
        ],
    );
    assert_eq!(
        states(&timeline),
        [
            (Phase::EnsureService, StepState::Failed),
            (Phase::EnsureService, StepState::Running),
        ]
    );
    assert_eq!(timeline.steps[0].retries, 2);
}

#[test]
fn only_a_run_whose_steps_all_succeeded_compares_with_the_usual() {
    let run = |last: &str, state: OperationState| {
        let mut timeline = timeline(&[
            (Phase::BuildGit, "web", 30_000),
            (Phase::EnsureService, "web", 2_000),
        ]);
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::BuildGit, "web", 0),
                action("action_succeeded", "a1", Phase::BuildGit, "web", 5_000),
                action("action_started", "a2", Phase::EnsureService, "web", 5_000),
                action(last, "a2", Phase::EnsureService, "web", 6_000),
            ],
        );
        timeline.operation.state = state;
        timeline.comparable_usual_ms()
    };
    assert_eq!(
        run("action_succeeded", OperationState::Succeeded),
        Some(32_000)
    );
    // A run that failed after 6 s is not 26 s faster than usual, even
    // when it failed after its last step succeeded.
    assert_eq!(run("action_failed", OperationState::Failed), None);
    assert_eq!(run("action_succeeded", OperationState::Failed), None);
    // Nor is one with a step it never ran before, whose usual time would
    // only count its known steps.
    let mut timeline = timeline(&[(Phase::BuildGit, "web", 30_000)]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "web", 0),
            action("action_succeeded", "a1", Phase::BuildGit, "web", 30_000),
            action("action_started", "a2", Phase::RunJob, "migrate", 30_000),
            action("action_succeeded", "a2", Phase::RunJob, "migrate", 90_000),
        ],
    );
    timeline.operation.state = OperationState::Succeeded;
    assert_eq!(timeline.usual_ms(), Some(30_000));
    assert_eq!(timeline.comparable_usual_ms(), None);
}

#[test]
fn sources_count_once_towards_the_time_left() {
    let mut timeline = timeline(&[
        (Phase::BuildGit, "api", 30_000),
        (Phase::BuildGit, "web", 20_000),
        (Phase::RunJob, "migrate", 5_000),
    ]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "api", 0),
            action("action_started", "a2", Phase::BuildGit, "web", 0),
        ],
    );
    // Usually the longest source's 30 s, then the job's 5 s.
    assert_eq!(timeline.usual_ms(), Some(35_000));
    // The longest source has 20 s left, then the job's 5 s.
    assert_eq!(timeline.remaining_ms(10_000), Some(25_000));
    // A step running over its expectation counts as about to end.
    assert_eq!(timeline.remaining_ms(40_000), Some(5_000));
}

fn rollout(at: i64, tasks: &[(&str, bool, TaskState)]) -> Event {
    Event {
        kind: "action_progressed".into(),
        action_id: Some("wait".into()),
        created_at_ms: at,
        detail: Some(EventDetail::Rollout {
            tasks: tasks
                .iter()
                .map(|(id, current, state)| RolloutTask {
                    id: (*id).into(),
                    slot: Some(1),
                    current: *current,
                    state: *state,
                    healthy: None,
                })
                .collect(),
            monitor_seconds: 5,
        }),
        ..Event::default()
    }
}

#[test]
fn rollouts_record_each_task_state_and_when_none_served() {
    use TaskState::{Preparing, Running, Shutdown};
    let mut timeline = timeline(&[]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "wait", Phase::WaitForService, "web", 0),
            // Stop-first: the old task stops before the new one runs.
            rollout(100, &[("old", false, Running)]),
            rollout(200, &[("old", false, Shutdown), ("new", true, Preparing)]),
            rollout(300, &[("old", false, Shutdown), ("new", true, Preparing)]),
            rollout(900, &[("old", false, Shutdown), ("new", true, Running)]),
        ],
    );
    let step = &timeline.steps[0];
    assert_eq!(step.monitor_seconds, Some(5));
    let lanes: Vec<_> = step
        .tasks
        .iter()
        .map(|lane| {
            let spans: Vec<_> = lane
                .spans
                .iter()
                .map(|span| (span.state, span.from_ms))
                .collect();
            (lane.task.id.as_str(), lane.task.current, spans)
        })
        .collect();
    assert_eq!(
        lanes,
        [
            ("old", false, vec![(Running, 100), (Shutdown, 200)]),
            ("new", true, vec![(Preparing, 200), (Running, 900)]),
        ]
    );
    assert_eq!(
        step.gaps(),
        [Gap {
            from_ms: 200,
            to_ms: Some(900)
        }]
    );

    // Start-first: the old task serves until the new one does.
    let mut start_first = self::timeline(&[]);
    apply(
        &mut start_first,
        vec![
            action("action_started", "wait", Phase::WaitForService, "web", 0),
            rollout(100, &[("old", false, Running), ("new", true, Preparing)]),
            rollout(500, &[("old", false, Running), ("new", true, Running)]),
            rollout(600, &[("old", false, Shutdown), ("new", true, Running)]),
        ],
    );
    assert_eq!(start_first.steps[0].gaps(), []);
    assert!(start_first.steps[0].overlapped());
    // A stop-first rollout neither overlaps, nor, when its handover
    // fell between two observations, shows a gap.
    assert!(!timeline.steps[0].overlapped());
    let mut unseen = self::timeline(&[]);
    apply(
        &mut unseen,
        vec![
            action("action_started", "wait", Phase::WaitForService, "web", 0),
            rollout(100, &[("old", false, Running)]),
            rollout(500, &[("old", false, Shutdown), ("new", true, Running)]),
        ],
    );
    assert_eq!(
        (unseen.steps[0].gaps(), unseen.steps[0].overlapped()),
        (vec![], false)
    );
}

#[test]
fn rollouts_summarize_their_new_tasks_monitor_window_and_downtime() {
    use TaskState::{Preparing, Running, Shutdown};
    let mut timeline = timeline(&[]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "wait", Phase::WaitForService, "web", 0),
            rollout(100, &[("old", false, Running)]),
            rollout(200, &[("old", false, Shutdown), ("new", true, Preparing)]),
        ],
    );
    let step = &timeline.steps[0];
    let tasks = step.new_tasks().unwrap();
    assert_eq!(
        tasks,
        NewTasks {
            running: 0,
            total: 1
        }
    );
    assert_eq!(tasks.to_string(), "0/1 new task running");
    // Down since the old task stopped, and still.
    let down = Downtime {
        ms: 800,
        ongoing: true,
    };
    assert_eq!(step.downtime(1_000), Some(down));
    assert_eq!(step.monitor(1_000), None);

    apply(
        &mut timeline,
        vec![rollout(
            2_000,
            &[("old", false, Shutdown), ("new", true, Running)],
        )],
    );
    let step = &timeline.steps[0];
    assert_eq!(step.new_tasks().unwrap().running, 1);
    // Swarm watches the new task for 5 s from when it ran.
    let window = Window {
        from_ms: 2_000,
        to_ms: 7_000,
    };
    assert_eq!(step.monitor(3_000), Some(window));
    assert_eq!(window.left_ms(3_000), 4_000);
    assert_eq!(step.monitor(7_000), None);
    let down = Downtime {
        ms: 1_800,
        ongoing: false,
    };
    assert_eq!(step.downtime(9_000), Some(down));

    // Once the step ended, no window is open.
    apply(
        &mut timeline,
        vec![action(
            "action_succeeded",
            "wait",
            Phase::WaitForService,
            "web",
            4_000,
        )],
    );
    assert_eq!(timeline.steps[0].monitor(3_000), None);
}

#[test]
fn another_attempt_starts_over() {
    let mut timeline = timeline(&[]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "web", 1),
            action("action_failed", "a1", Phase::BuildGit, "web", 2),
        ],
    );
    // Requested again, its latest attempt is the run before.
    let mut requested = timeline.operation.clone();
    requested.state = OperationState::Requested;
    timeline.set_operation(requested.clone());
    assert_eq!(timeline.steps.len(), 0, "{:?}", timeline.steps);
    let mut retried = requested;
    retried.state = OperationState::Running;
    retried.attempt = 2;
    timeline.set_operation(retried);
    // Events of the earlier attempt no longer apply.
    let mut late = action("action_started", "a3", Phase::EnsureService, "web", 3);
    late.id = timeline.cursor + 1;
    late.operation_id = Some("op".into());
    late.attempt = Some(1);
    timeline.apply(&late);
    assert_eq!(timeline.steps.len(), 0);
}

#[test]
fn a_requested_operation_shows_none_of_its_previous_run() {
    let mut timeline = timeline(&[(Phase::EnsureService, "web", 2_000)]);
    timeline.operation.state = OperationState::Requested;
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::EnsureService, "web", 1),
            action("action_succeeded", "a1", Phase::EnsureService, "web", 2),
        ],
    );
    assert_eq!(
        states(&timeline),
        [(Phase::EnsureService, StepState::Forecast)]
    );
}

#[test]
fn only_noticeably_unusual_durations_are_flagged() {
    let mut timeline = timeline(&[
        (Phase::BuildGit, "api", 30_000),
        (Phase::BuildGit, "web", 20_000),
        (Phase::EnsureService, "api", 1_000),
    ]);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "api", 0),
            action("action_started", "a2", Phase::BuildGit, "web", 0),
            action("action_succeeded", "a1", Phase::BuildGit, "api", 50_000),
            action("action_succeeded", "a2", Phase::BuildGit, "web", 21_000),
            action("action_started", "a3", Phase::EnsureService, "api", 50_000),
        ],
    );
    let unusual: Vec<_> = timeline.steps.iter().map(Step::unusual_ms).collect();
    // 20 s over its 30 s is unusual; 1 s over 20 s is not.
    assert_eq!(unusual, [Some(30_000), None, None]);
    // The sources build at once, then the service updates.
    assert_eq!(timeline.usual_ms(), Some(31_000));
}

#[test]
fn events_already_applied_or_of_other_operations_are_ignored() {
    let mut timeline = timeline(&[]);
    apply(
        &mut timeline,
        vec![action(
            "action_started",
            "a1",
            Phase::EnsureService,
            "web",
            1,
        )],
    );
    let mut replayed = action("action_succeeded", "a1", Phase::EnsureService, "web", 2);
    replayed.id = timeline.cursor;
    replayed.operation_id = Some("op".into());
    timeline.apply(&replayed);
    let mut other = action("action_succeeded", "a1", Phase::EnsureService, "web", 2);
    other.id = timeline.cursor + 1;
    other.operation_id = Some("other".into());
    timeline.apply(&other);
    assert_eq!(timeline.steps[0].state, StepState::Running);
}

#[test]
fn progress_counts_from_the_first_start_and_rounds_down() {
    let mut timeline = timeline(&[
        (Phase::BuildGit, "web", 30_000),
        (Phase::RunJob, "migrate", 4_000),
        (Phase::EnsureService, "web", 2_000),
    ]);
    timeline.operation.state = OperationState::Requested;
    timeline.operation.started_at_ms = None;
    let queued = Progress::new(&timeline, 1_000);
    assert_eq!(
        (queued.status(), queued.steps(), queued.percent()),
        ("Queued", "0/3".into(), 0)
    );
    assert_eq!(queued.elapsed_ms, None);

    // The operation's own start trails its first step's.
    timeline.operation.state = OperationState::Running;
    timeline.operation.started_at_ms = Some(2_000);
    apply(
        &mut timeline,
        vec![
            action("action_started", "a1", Phase::BuildGit, "web", 1_000),
            action("action_succeeded", "a1", Phase::BuildGit, "web", 11_000),
            action("action_started", "a2", Phase::RunJob, "migrate", 11_000),
        ],
    );
    let running = Progress::new(&timeline, 13_000);
    assert_eq!(running.status(), "Deploying");
    assert_eq!(running.running, ["Run job migrate"]);
    assert_eq!(running.elapsed_ms, Some(12_000));
    // One step of three: 33 %, and 4 of a 12-cell bar.
    assert_eq!((running.percent(), running.filled(12)), (33, 4));
    // The job has 2 s left, then the service's 2 s.
    assert_eq!(running.outlook().as_deref(), Some("~4.0 s left"));
}

#[test]
fn progress_of_no_steps_is_none_done() {
    let progress = Progress::new(&timeline(&[]), 0);
    assert_eq!((progress.percent(), progress.filled(12)), (0, 0));
    assert_eq!(progress.steps(), "0/0");
}

#[test]
fn progress_names_its_state_by_kind() {
    let mut timeline = timeline(&[]);
    let status = |timeline: &Timeline| Progress::new(timeline, 0).status();
    timeline.operation.kind = OperationKind::Delete;
    assert_eq!(status(&timeline), "Deleting");
    timeline.operation.state = OperationState::Succeeded;
    assert_eq!(status(&timeline), "Deleted");
    timeline.operation.kind = OperationKind::Apply;
    assert_eq!(status(&timeline), "Deployed");
    timeline.operation.state = OperationState::Superseded;
    assert_eq!(status(&timeline), "Superseded");
}

#[test]
fn progress_tells_the_time_left_then_against_the_usual() {
    let mut progress = Progress {
        kind: OperationKind::Apply,
        state: OperationState::Running,
        done: 4,
        total: 9,
        running: vec!["Build web".into()],
        elapsed_ms: Some(30_000),
        left_ms: Some(48_000),
        usual_ms: Some(80_000),
    };
    assert_eq!(progress.steps(), "4/9");
    assert_eq!(progress.outlook().as_deref(), Some("~48.0 s left"));
    progress.state = OperationState::Succeeded;
    for (elapsed, outlook) in [
        (74_000, "6.0 s faster than usual"),
        (79_001, "as long as usual"),
        (80_999, "as long as usual"),
        (81_000, "1.0 s slower than usual"),
    ] {
        progress.elapsed_ms = Some(elapsed);
        assert_eq!(progress.outlook().as_deref(), Some(outlook), "{elapsed}");
    }
    // A run that is not comparable with the usual one says nothing.
    progress.usual_ms = None;
    assert_eq!(progress.outlook(), None);
}
