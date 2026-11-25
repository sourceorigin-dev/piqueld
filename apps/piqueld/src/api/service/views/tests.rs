use std::collections::BTreeMap;

use piqueld_core::resource::{ObservedTask, TaskDiagnostic};

use super::*;

/// A one-replica `web` service running `tasks`.
fn service(tasks: Vec<ObservedTask>, healthcheck_configured: bool) -> ObservedService {
    ObservedService {
        secrets: Vec::new(),
        files: Vec::new(),
        name: "web".into(),
        image: "example/web@sha256:digest".into(),
        replicas: 1,
        environment: BTreeMap::new(),
        command: Vec::new(),
        arguments: Vec::new(),
        mounts: Vec::new(),
        healthcheck: None,
        healthcheck_configured,
        resources: None,
        networks: Vec::new(),
        labels: BTreeMap::new(),
        rollout: None,
        telemetry: None,
        security: piqueld_core::manifest::ServiceSecurity::default(),
        runtime_configuration_matches: true,
        tasks,
        convergence: Convergence::Converged,
        update_message: None,
    }
}

#[test]
fn service_diagnostics_ignore_historical_tasks() {
    let service = service(
        vec![
            ObservedTask {
                state: TaskState::Failed,
                healthy: None,
                desired_running: false,
                diagnostic: Some(TaskDiagnostic::Failed {
                    exit_code: Some(1),
                    reason: None,
                }),
                ..ObservedTask::default()
            },
            ObservedTask {
                state: TaskState::Rejected,
                healthy: None,
                desired_running: true,
                diagnostic: Some(TaskDiagnostic::Rejected { reason: None }),
                ..ObservedTask::default()
            },
        ],
        false,
    );

    let diagnostics = service_diagnostics(&service);

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, ApiErrorCode::TaskRejected);
}

/// A failed service gives Docker's reason even once Swarm replaced the
/// failed task, and a desired task's row gives its own.
#[test]
fn service_diagnostics_give_dockers_reason() {
    let rejected = |desired_running, reason| ObservedTask {
        state: TaskState::Rejected,
        desired_running,
        current: true,
        diagnostic: Some(TaskDiagnostic::Rejected {
            reason: RuntimeMessage::new(reason),
        }),
        ..ObservedTask::default()
    };
    let mut service = service(vec![rejected(false, "No such image: web:2")], false);
    service.convergence = Convergence::Failed;
    let messages = |service: &ObservedService| {
        service_diagnostics(service)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        messages(&service),
        ["0 of 1 observed replicas are healthy: No such image: web:2"]
    );
    service.tasks.push(rejected(true, "invalid mount config"));
    assert_eq!(
        messages(&service),
        [
            "0 of 1 observed replicas are healthy: invalid mount config",
            "the runtime rejected a desired task before it started: invalid mount config",
        ]
    );
}

#[test]
fn pending_runtime_health_is_not_reported_as_healthy() {
    let task = ObservedTask {
        state: TaskState::Running,
        healthy: None,
        desired_running: true,
        diagnostic: None,
        ..ObservedTask::default()
    };
    let mut service = service(vec![task], true);

    assert_eq!(healthy_replicas(&service), 0);
    service.healthcheck_configured = false;
    assert_eq!(healthy_replicas(&service), 1);
}
