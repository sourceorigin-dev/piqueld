//! API records the overview's tests are built from.
use super::{ApplicationRow, EnvironmentRow};
use piqueld_client::{ApplicationState, EnvironmentId, EnvironmentStatusView};
use serde_json::json;

/// An environment named `name`, `stopped` or running, with `status`, if one
/// was recorded.
pub(super) fn environment(
    name: &str,
    stopped: bool,
    status: Option<(ApplicationState, Option<&str>)>,
) -> EnvironmentRow {
    let id = format!("{name}-environment");
    EnvironmentRow {
        status: status.map(|(state, message)| EnvironmentStatusView {
            environment_id: EnvironmentId::parse(id.clone()).expect("the environment id parses"),
            state,
            runtime_health: None,
            health_observed_at_ms: None,
            message: message.map(str::to_owned),
            updated_at_ms: 0,
        }),
        environment: serde_json::from_value(json!({
            "id": id,
            "application_id": "app-shop0001",
            "name": name,
            "source": { "type": "saved" },
            "run_state": if stopped { "stopped" } else { "running" },
            "resolved_generation": null,
            "delete_intent": false,
            "created_at_ms": 0,
            "updated_at_ms": 0,
        }))
        .expect("environment"),
    }
}

/// Application `id` with `environments`, and one latest deployment per
/// `(operation ID, requested at)`.
pub(super) fn application(
    id: &str,
    environments: Vec<EnvironmentRow>,
    deployments: &[(&str, i64)],
) -> ApplicationRow {
    let deployment = |(operation, created_at_ms): &(&str, i64)| {
        serde_json::from_value(json!({
            "id": operation,
            "environment_id": id,
            "kind": "apply",
            "generation": 1,
            "attempt": 1,
            "state": "requested",
            "error_code": null,
            "error_message": null,
            "created_at_ms": created_at_ms,
            "updated_at_ms": created_at_ms,
            "started_at_ms": null,
            "finished_at_ms": null,
        }))
        .expect("deployment")
    };
    ApplicationRow {
        application: serde_json::from_value(json!({
            "id": id,
            "name": id,
            "generation": 4,
            "delete_intent": false,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "environments": [],
        }))
        .expect("application"),
        environments,
        deployments: deployments.iter().map(deployment).collect(),
    }
}
