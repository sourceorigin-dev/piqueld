use super::*;
use piqueld_client::{OperationKind, OperationState};
use serde_json::{Value, json};

/// The manifest every fixture deploys.
fn template() -> Value {
    json!({
        "id": "app-0123456789abcdef0123456789abcdef",
        "api_version": "piqueld.dev/v1alpha1",
        "kind": "Application",
        "metadata": { "name": "shop" },
        "spec": { "services": [{ "name": "web", "source": { "type": "image", "image": "nginx:alpine" } }] },
    })
}

/// Attempt `attempt` of operation `id`, in `state`.
fn operation(id: &str, attempt: u64, state: OperationState) -> Operation {
    Operation {
        id: id.into(),
        environment_id: "env-0123456789abcdef0123456789abcdef".parse().unwrap(),
        kind: OperationKind::Refresh,
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

/// Deployment `id`, with its `current_target` and `last_successful` flags.
fn deployment(id: &str, current: bool, successful: bool) -> DeploymentView {
    serde_json::from_value(json!({
        "operation": operation(id, 1, OperationState::Succeeded),
        "template": template(),
        "variables": {},
        "application": null,
        "succeeded_at_ms": 1,
        "current_target": current,
        "last_successful": successful,
    }))
    .unwrap()
}

/// Deployment `id`'s summary, with its flags.
fn summary(id: &str, current: bool, successful: bool) -> DeploymentSummary {
    deployment(id, current, successful).into()
}

fn ids(list: &[DeploymentSummary]) -> Vec<&str> {
    list.iter().map(|item| item.operation.id.as_str()).collect()
}

#[test]
fn a_row_names_the_commit_deployed_and_how_long_it_ran() {
    let mut built = deployment("operation-1", true, true);
    assert_eq!((built.commit(), built.duration_ms()), (None, None));
    built.variables = serde_json::from_value(json!({
        "git.branch": "main",
        "git.sha": "0123456789abcdef0123456789abcdef01234567",
    }))
    .unwrap();
    built.operation.created_at_ms = 1_000;
    built.operation.finished_at_ms = Some(39_000);
    assert_eq!(built.commit().as_deref(), Some("main @ 0123456789ab"));
    assert_eq!(built.duration_ms(), Some(38_000));
}

/// Only another deployment, the environment's latest, supersedes one: not
/// the deployment itself, a deletion, or another environment's operation.
#[test]
fn only_the_latest_deployment_of_its_environment_supersedes_one() {
    let current = deployment("operation-1", true, true);
    assert!(!current.superseded_by(None));
    assert!(!current.superseded_by(Some(&current.operation)));
    let failed = operation("operation-0", 1, OperationState::Failed);
    assert!(current.superseded_by(Some(&failed)));
    let deletion = Operation {
        kind: OperationKind::Delete,
        ..failed.clone()
    };
    assert!(!current.superseded_by(Some(&deletion)));
    let elsewhere = Operation {
        environment_id: "env-fedcba9876543210fedcba9876543210".parse().unwrap(),
        ..failed
    };
    assert!(!current.superseded_by(Some(&elsewhere)));
}

#[test]
fn a_polled_newest_page_keeps_older_pages_and_moves_the_flags() {
    let mut list = vec![
        summary("operation-2", true, true),
        summary("operation-1", false, false),
    ];
    list.append_older(vec![
        summary("operation-1", false, false),
        summary("operation-0", false, false),
    ]);
    assert_eq!(ids(&list), ["operation-2", "operation-1", "operation-0"]);

    let flags = |list: &[DeploymentSummary]| {
        list.iter()
            .map(|item| (item.current_target, item.last_successful))
            .collect::<Vec<_>>()
    };
    // A newer deployment became the current target, but hasn't succeeded
    // yet: deployment 2, off the polled page, stays the last successful.
    list.merge_newest(vec![summary("operation-3", true, false)]);
    assert_eq!(
        ids(&list),
        ["operation-3", "operation-2", "operation-1", "operation-0"]
    );
    assert_eq!(
        flags(&list),
        [(true, false), (false, true), (false, false), (false, false)]
    );
    // Then a newer one succeeded without becoming the current target.
    list.merge_newest(vec![summary("operation-4", false, true)]);
    assert_eq!(
        flags(&list)[..3],
        [(false, true), (true, false), (false, false)]
    );
}

#[test]
fn older_attempt_pages_append_and_a_refresh_shows_the_newest_alone() {
    let mut attempts = Attempts::default();
    let retained = |attempt| operation("operation-1", attempt, OperationState::Failed);
    let page = |attempts: &[u64], next: Option<&str>| Page {
        items: attempts.iter().copied().map(retained).collect(),
        next_cursor: next.map(str::to_owned),
    };
    let numbers = |attempts: &Attempts| {
        attempts
            .items
            .iter()
            .map(|item| item.attempt)
            .collect::<Vec<_>>()
    };
    attempts.show(page(&[2], Some("older")), false);
    attempts.show(page(&[1], None), true);
    assert_eq!(
        (numbers(&attempts), attempts.cursor.clone()),
        (vec![2, 1], None)
    );
    // The running attempt leads them (see `Operation::attempts`).
    let running = operation("operation-1", 3, OperationState::Running);
    assert_eq!(attempts.visible(&running).len(), 3);
    attempts.show(page(&[3], Some("older")), false);
    assert_eq!(
        (numbers(&attempts), attempts.cursor.as_deref()),
        (vec![3], Some("older"))
    );
}

/// A plan of promoting a release from `origin`, at revision 7, whose
/// images are `availability` and which lacks `secrets`.
fn plan(origin: &Value, availability: &Value, secrets: &Value) -> PlanView {
    serde_json::from_value(json!({
        "application_id": "app-0123456789abcdef0123456789abcdef",
        "generation": 7,
        "identical": false,
        "operation": null,
        "changes": [],
        "plan": { "actions": [], "diagnostics": [] },
        "rollouts": [],
        "release": {
            "release": {
                "id": "rel-0123456789abcdef0123456789abcdef",
                "application_id": "app-0123456789abcdef0123456789abcdef",
                "content_hash": format!("sha256:{}", "b".repeat(64)),
                "created_at_ms": 0,
                "release": {
                    "template": template(),
                    "sources": { "web": {
                        "type": "image",
                        "requested": "nginx:alpine",
                        "digest_reference": format!("docker.io/library/nginx@sha256:{}", "a".repeat(64)),
                    } },
                },
                "fingerprint": { "web": { "source.image": "nginx:alpine" } },
                "availability": availability,
            },
            "origin": origin,
            "new_volumes": [],
            "secrets": secrets,
        },
    }))
    .unwrap()
}

#[test]
fn a_promotion_confirms_exactly_the_reviewed_source_and_revision() {
    let promoted = json!({
        "type": "promotion",
        "environment": "env-0123456789abcdef0123456789abcdef",
        "deployment": "operation-1",
    });
    let (present, none) = (json!({ "state": "present" }), json!([]));
    let request = Promotion::new(None)
        .confirmed(&plan(&promoted, &present, &none))
        .unwrap();
    assert_eq!(request.deployment.as_deref(), Some("operation-1"));
    assert_eq!(request.expected_generation, Some(7));
    assert_eq!(request.release, None);

    // An earlier release is deployed as itself, not pinned to a deployment.
    let id = "rel-0123456789abcdef0123456789abcdef";
    let release = Promotion::new(Some(id.into()))
        .confirmed(&plan(&json!({ "type": "release" }), &present, &none))
        .unwrap();
    assert_eq!(release.deployment, None);
    assert_eq!(release.release.as_deref(), Some(id));

    // Missing secrets, or built images that are gone, block it; missing
    // registry images are pulled again by digest.
    let missing = json!([{ "problem": "missing", "secret": "api-key" }]);
    let confirmed = |availability: &str, secrets: &Value| {
        let availability = json!({ "state": availability, "missing": [] });
        Promotion::new(None)
            .confirmed(&plan(&promoted, &availability, secrets))
            .is_some()
    };
    assert!(!confirmed("present", &missing));
    assert!(!confirmed("unavailable", &none));
    assert!(confirmed("pullable", &none));
}

#[test]
fn a_blank_preview_slot_is_none() {
    let form = PreviewForm {
        branch: " feat/login ".into(),
        slot: "  ".into(),
    };
    let request = form.request();
    assert_eq!(request.branch, "feat/login");
    assert_eq!(request.slot, None);
    let slotted = PreviewForm {
        slot: " agent-2 ".into(),
        ..form
    };
    assert_eq!(slotted.request().slot.as_deref(), Some("agent-2"));
}
