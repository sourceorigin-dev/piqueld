//! Rolling services out after the services they depend on.

use piqueld_core::resource::{Convergence, ObservedApplication, ResolvedApplication};
use piqueld_core::{ApplicationId, parse_toml};

use super::{observed, reconcile_plan, resolve_all};

/// `server` depends on `db`; `web` is independent and sorts after both.
fn dependent_application() -> ResolvedApplication {
    let service = |name: &str, depends_on: &str| {
        format!(
            "[[spec.services]]\nname = \"{name}\"\ndepends_on = [{depends_on}]\n[spec.services.source]\ntype = \"image\"\nimage = \"ghcr.io/example/notes:1.4.0\"\n"
        )
    };
    let app = parse_toml(&format!(
        "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n{}{}{}",
        service("db", ""),
        service("server", "\"db\""),
        service("web", ""),
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    resolve_all(&app)
}

/// Observes `desired` with `db` in `db_state` and the `missing` services absent.
fn dependent_observation(
    desired: &ResolvedApplication,
    db_state: Convergence,
    missing: &[&str],
) -> ObservedApplication {
    let mut observation = observed(desired);
    for (service, observed) in desired.services.iter().zip(&mut observation.services) {
        if service.logical_name.as_str() == "db" {
            observed.convergence = db_state;
        }
    }
    observation.services.retain(|observed| {
        desired.services.iter().all(|service| {
            service.name.as_str() != observed.name
                || !missing.contains(&service.logical_name.as_str())
        })
    });
    observation
}

/// Service actions as `action logical-name`, in plan order.
fn service_steps(desired: &ResolvedApplication, observed: &ObservedApplication) -> Vec<String> {
    reconcile_plan(desired, observed)
        .actions
        .iter()
        .filter(|action| {
            desired
                .services
                .iter()
                .any(|service| service.logical_name.as_str() == action.kind.resource_name())
        })
        .map(|action| format!("{} {}", action.kind.name(), action.kind.resource_name()))
        .collect()
}

#[test]
fn dependents_roll_out_only_after_their_dependencies_converge() {
    let desired = dependent_application();
    // The controller executes the first action and replans, so `server` cannot
    // start before the wait for `db` succeeds. Independent services do not wait.
    assert_eq!(
        service_steps(&desired, &ObservedApplication::default()),
        [
            "ensure_service db",
            "ensure_service web",
            "wait_for_service db",
            "ensure_service server",
            "wait_for_service server",
            "wait_for_service web",
        ]
    );
    assert_eq!(
        service_steps(
            &desired,
            &dependent_observation(&desired, Convergence::Updating, &["server"])
        ),
        [
            "wait_for_service db",
            "ensure_service server",
            "wait_for_service server"
        ]
    );
    assert_eq!(
        service_steps(
            &desired,
            &dependent_observation(&desired, Convergence::Converged, &["server"])
        ),
        ["ensure_service server", "wait_for_service server"]
    );
    // A converged dependent does not hold drift repair behind its dependency.
    assert_eq!(
        service_steps(
            &desired,
            &dependent_observation(&desired, Convergence::Degraded, &["web"])
        ),
        [
            "ensure_service web",
            "wait_for_service db",
            "wait_for_service web"
        ]
    );
}

#[test]
fn stopped_dependents_scale_down_without_waiting_for_their_dependencies() {
    let running = dependent_application();
    let stopped = running.clone().with_stopped(true);
    // `db` is still shutting down while `server` and `web` run.
    let mut observation = dependent_observation(&running, Convergence::Updating, &[]);
    for (service, observed) in running.services.iter().zip(&mut observation.services) {
        if service.logical_name.as_str() == "db" {
            observed.replicas = 0;
        }
    }
    assert_eq!(
        service_steps(&stopped, &observation),
        [
            "ensure_service server",
            "ensure_service web",
            "wait_for_service db",
            "wait_for_service server",
            "wait_for_service web",
        ]
    );
}

#[test]
fn active_repair_stops_at_a_dependency_still_converging() {
    let desired = dependent_application();
    let repair = |missing: &[&str]| {
        reconcile_plan(
            &desired,
            &dependent_observation(&desired, Convergence::Degraded, missing),
        )
        .next_repair()
        .map(|action| action.kind.resource_name().to_owned())
    };
    assert_eq!(repair(&["server"]), None);
    assert_eq!(repair(&["server", "web"]), Some("web".to_owned()));
}
