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
                diagnostic: Some(TaskDiagnostic::Failed { exit_code: Some(1) }),
                ..ObservedTask::default()
            },
            ObservedTask {
                state: TaskState::Rejected,
                healthy: None,
                desired_running: true,
                diagnostic: Some(TaskDiagnostic::Rejected),
                ..ObservedTask::default()
            },
        ],
        false,
    );

    let diagnostics = service_diagnostics(&service);

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, ApiErrorCode::TaskRejected);
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
