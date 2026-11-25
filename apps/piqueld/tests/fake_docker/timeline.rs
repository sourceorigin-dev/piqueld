//! Operation timelines: the steps a deployment ran, folded from its events,
//! and the forecast its environment's next deployment starts from.

use super::*;
use piqueld_core::Phase;
use piqueld_core::timeline::{StepKey, StepState};

/// The steps of `timeline`, as phase, resource and state.
fn steps(timeline: &piqueld_core::timeline::Timeline) -> Vec<(StepKey, StepState)> {
    timeline
        .steps
        .iter()
        .map(|step| (step.key.clone(), step.state))
        .collect()
}

#[tokio::test]
async fn deployments_record_their_steps_and_forecast_the_next_one() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    let done = harness
        .applications()
        .timeline(&first.id)
        .await
        .expect("timeline succeeds");
    assert_eq!(done.operation.state, OperationState::Succeeded);
    assert!(
        done.steps
            .iter()
            .all(|step| step.state == StepState::Succeeded && step.finished_at_ms.is_some()),
        "{done:#?}"
    );
    let phases: Vec<_> = done.steps.iter().map(|step| step.key.phase).collect();
    for phase in [Phase::ResolveImage, Phase::EnsureService] {
        assert!(phases.contains(&phase), "{phases:?}");
    }

    let second = harness.apply(manifest(), Some(1)).await;
    let forecast = harness
        .applications()
        .timeline(&second.id)
        .await
        .expect("timeline succeeds");
    assert_eq!(
        steps(&forecast),
        done.steps
            .iter()
            .map(|step| (step.key.clone(), StepState::Forecast))
            .collect::<Vec<_>>()
    );
    assert!(forecast.steps.iter().all(|step| step.expected_ms.is_some()));
    assert!(
        forecast
            .remaining_ms(forecast.operation.created_at_ms)
            .is_some()
    );

    harness.finish(&second).await;
    let deployed = harness
        .applications()
        .timeline(&second.id)
        .await
        .expect("timeline succeeds");
    assert!(
        deployed
            .steps
            .iter()
            .all(|step| step.state == StepState::Succeeded),
        "{deployed:#?}"
    );
}

#[tokio::test]
async fn a_timeline_followed_across_a_rerun_shows_what_a_new_read_does() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .docker
        .fail_convergence
        .store(true, Ordering::SeqCst);
    let mut scaled = manifest();
    for service in &mut scaled.spec.services {
        service.replicas = 2.into();
    }
    let failed = harness.apply(scaled, None).await;
    assert_eq!(harness.scan_result(&failed).await.0, OperationState::Failed);
    let applications = harness.applications();
    let mut followed = applications
        .timeline(&failed.id)
        .await
        .expect("timeline succeeds");

    harness
        .docker
        .fail_convergence
        .store(false, Ordering::SeqCst);
    let rerun = applications
        .reconcile(&failed.environment_id, None)
        .await
        .expect("reconcile succeeds");
    assert_eq!(rerun.id, failed.id);
    assert!(
        applications
            .advance_timeline(&mut followed)
            .await
            .expect("advance timeline succeeds")
    );
    let read = applications
        .timeline(&failed.id)
        .await
        .expect("timeline succeeds");
    assert_eq!(followed.operation.attempt, read.operation.attempt);
    assert_eq!(steps(&followed), steps(&read));
}

#[tokio::test]
async fn waiting_for_a_rollout_records_its_tasks_as_they_start_and_run() {
    use piqueld_core::resource::TaskState;
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .slow_services
        .lock()
        .await
        .insert("web".into(), 3);
    let operation = harness.apply(manifest(), Some(0)).await;
    harness.finish(&operation).await;
    let timeline = harness
        .applications()
        .timeline(&operation.id)
        .await
        .expect("timeline succeeds");
    let wait = timeline
        .steps
        .iter()
        .find(|step| step.key.phase == Phase::WaitForService)
        .expect("the deployment waited for web");
    assert!(!wait.tasks.is_empty(), "{wait:#?}");
    for lane in &wait.tasks {
        assert!(lane.task.current);
        let states: Vec<_> = lane.spans.iter().map(|span| span.state).collect();
        assert_eq!(states, [TaskState::Starting, TaskState::Running]);
    }
    assert!(wait.monitor_seconds.is_some());
    assert_eq!(wait.gaps(), []);
}
