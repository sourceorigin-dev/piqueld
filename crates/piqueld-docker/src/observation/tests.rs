use super::*;

fn running(healthy: Option<bool>) -> ObservedTask {
    ObservedTask {
        state: TaskState::Running,
        healthy,
        desired_running: true,
        diagnostic: None,
        ..ObservedTask::default()
    }
}

/// Tasks created since the service's latest update started run its
/// specification, and so do all tasks of a service never updated.
#[test]
fn tasks_created_since_the_latest_update_are_current() {
    let current = |created: &str, updated: Option<&str>| {
        let task = bollard::models::Task {
            created_at: Some(created.into()),
            ..Default::default()
        };
        BollardDocker::observe_task(&task, updated, None).current
    };
    let updated = Some("2026-11-10T16:11:28.5Z");
    assert!(current("2026-11-10T16:11:28.500000001Z", updated));
    assert!(current("2026-11-10T16:11:29Z", updated));
    assert!(!current("2026-11-10T16:11:28.38968Z", updated));
    assert!(!current("2026-11-10T16:11:28Z", updated));
    assert!(current("2026-11-10T16:11:28Z", None));
    assert!(!current("yesterday", updated));
}

/// A failed or rejected task keeps Docker's error, cleaned, but not the
/// status message, which names the last step the task reached.
#[test]
fn failed_tasks_keep_dockers_error() {
    let task = |state, err: Option<&str>| bollard::models::Task {
        status: Some(bollard::models::TaskStatus {
            state: Some(state),
            message: Some("started".into()),
            err: err.map(Into::into),
            ..Default::default()
        }),
        ..Default::default()
    };
    let diagnostic = |task| BollardDocker::observe_task(&task, None, None).diagnostic;
    assert_eq!(
        diagnostic(task(
            bollard::models::TaskState::REJECTED,
            Some("No such image:\n example/web:2")
        )),
        Some(TaskDiagnostic::Rejected {
            reason: RuntimeMessage::new("No such image: example/web:2"),
        })
    );
    assert_eq!(
        diagnostic(task(bollard::models::TaskState::FAILED, None)),
        Some(TaskDiagnostic::Failed {
            exit_code: None,
            reason: None,
        })
    );
}

/// Only a paused update's message explains a failure.
#[test]
fn only_paused_updates_keep_their_message() {
    let message = |state| {
        BollardDocker::paused_update_message(bollard::models::ServiceUpdateStatus {
            state: Some(state),
            message: Some("update paused due to failure".into()),
            ..Default::default()
        })
    };
    assert_eq!(
        message(bollard::models::ServiceUpdateStatusStateEnum::PAUSED),
        RuntimeMessage::new("update paused due to failure")
    );
    assert_eq!(
        message(bollard::models::ServiceUpdateStatusStateEnum::COMPLETED),
        None
    );
}

/// Swarm reports rejected tasks with an empty container ID; reading
/// `/containers//logs` for one failed every log request with a 301.
#[test]
fn tasks_without_a_container_have_no_container_id() {
    let task = |container_id: Option<&str>| bollard::models::Task {
        status: Some(bollard::models::TaskStatus {
            container_status: Some(bollard::models::ContainerStatus {
                container_id: container_id.map(Into::into),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(BollardDocker::task_container(&task(Some(""))), None);
    assert_eq!(BollardDocker::task_container(&task(None)), None);
    assert_eq!(
        BollardDocker::task_container(&task(Some("9472413"))),
        Some("9472413")
    );
}

#[test]
fn stale_ingress_proxies_are_drift() {
    let spec = ServiceSpec {
        name: Some("app-web".into()),
        task_template: Some(bollard::models::TaskSpec {
            container_spec: Some(TaskSpecContainerSpec {
                env: Some(vec![format!("{INGRESS_PROXIES_ENV}=10.0.1.0/24")]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let observe = |expected| {
        let mut service = BollardDocker::observe_service(&spec, "node", Vec::new(), None)
            .expect("observed service");
        service.runtime_configuration_matches = true;
        BollardDocker::observe_ingress_proxies(&mut service, expected);
        assert!(service.environment.is_empty());
        service.runtime_configuration_matches
    };
    assert!(observe(BollardDocker::spec_ingress_proxies(&spec)));
    assert!(!observe(Some("10.0.2.0/24")));
    assert!(!observe(None));
}

/// Running tasks converge once healthy when a health check is
/// configured, and a service scaled to zero once its tasks stopped.
#[test]
fn convergence_follows_task_states_and_health() {
    let stopping = ObservedTask {
        desired_running: false,
        ..running(None)
    };
    for (tasks, replicas, healthcheck, expected) in [
        (vec![running(None)], 1, true, Convergence::Updating),
        (vec![running(Some(true))], 1, true, Convergence::Converged),
        (vec![running(None)], 1, false, Convergence::Converged),
        (vec![stopping], 0, false, Convergence::Updating),
        (vec![], 0, false, Convergence::Converged),
    ] {
        assert_eq!(
            BollardDocker::convergence(&tasks, replicas, None, healthcheck),
            expected,
            "{tasks:?} {healthcheck}"
        );
    }
    let disabled = HealthConfig {
        test: Some(vec!["NONE".into()]),
        ..Default::default()
    };
    assert!(!BollardDocker::healthcheck_configured(&disabled));
}
