//! Which desired fields an observed service drifts from.

use piqueld_core::Plan;
use piqueld_core::manifest::{RolloutOrder, ValidatedRollout as Rollout};
use piqueld_core::planner::{ActionReason, PlanRequest};
use piqueld_core::resource::{ObservedService, compile_application};

use super::{application, environment, instance, observed, resolutions};

#[test]
fn changed_rollout_settings_drift_until_applied() {
    let mut desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
    let converged = observed(&desired).services.remove(0);
    assert_eq!(converged.drift(&desired.services[0]), [] as [&str; 0]);

    desired.services[0].rollout = Rollout {
        order: Some(RolloutOrder::StartFirst),
        monitor_seconds: Some(10),
    };
    assert_eq!(converged.drift(&desired.services[0]), ["rollout"]);
    let updated = observed(&desired).services.remove(0);
    assert_eq!(updated.drift(&desired.services[0]), [] as [&str; 0]);

    // Docker reporting a policy piqueld cannot express is drift too.
    let unexpressible = ObservedService {
        rollout: None,
        ..updated
    };
    assert_eq!(unexpressible.drift(&desired.services[0]), ["rollout"]);
}

/// Only the receiving service gets the telemetry mount, which a granted
/// environment projects and a revoked one removes, as drift both ways.
#[test]
fn telemetry_mounts_follow_the_grant_onto_the_receiver_only() {
    let mut desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
    let unmounted = observed(&desired).services.remove(0);
    desired.services[0].receives_telemetry = true;
    let mount = piqueld_core::resource::TelemetryMount {
        source: "/var/lib/piqueld/telemetry/app-notes-01".into(),
        user: "990:990".into(),
    };
    let granted = desired.clone().with_telemetry(Some(&mount));
    assert_eq!(granted.services[0].telemetry.as_ref(), Some(&mount));
    assert_eq!(unmounted.drift(&granted.services[0]), ["telemetry"]);
    let mounted = observed(&granted).services.remove(0);
    let revoked = granted.with_telemetry(None);
    assert_eq!(revoked.services[0].telemetry, None);
    assert_eq!(mounted.drift(&revoked.services[0]), ["telemetry"]);

    desired.services[0].receives_telemetry = false;
    assert_eq!(
        desired.with_telemetry(Some(&mount)).services[0].telemetry,
        None
    );
}

#[test]
fn drift_fields_are_granular_for_command_and_arguments() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let mut drifted = observed(&desired);
    drifted.services[0].command = vec!["sh".into()];
    drifted.services[0].arguments = vec!["-c".into(), "true".into()];
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &drifted,
    );
    let reasons = plan
        .actions
        .iter()
        .filter_map(|action| match &action.reason {
            ActionReason::Drift { fields } => Some(fields.clone()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"command".to_string()), "{reasons:?}");
    assert!(reasons.contains(&"arguments".to_string()), "{reasons:?}");
    assert!(!reasons.contains(&"process".to_string()));
}

#[test]
fn unsupported_observed_healthcheck_is_healthcheck_drift() {
    let app = application();
    let mut desired =
        compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    desired.services[0].healthcheck = None;
    // Docker reports a health check that piqueld cannot represent.
    let mut drifted = observed(&desired);
    drifted.services[0].healthcheck_configured = true;
    assert_eq!(
        drifted.services[0].drift(&desired.services[0]),
        ["healthcheck"]
    );
}

#[test]
fn observation_matching_ignores_order_but_not_multiplicity() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();

    // Order is irrelevant for networks and mounts alike...
    let mut reordered = observed(&desired);
    reordered.services[0].mounts.reverse();
    assert!(reordered.services[0].matches(&desired.services[0]));

    // ...but multiplicity is preserved: duplicated attachments are drift.
    let mut duplicated = observed(&desired);
    duplicated.services[0].networks = desired.services[0]
        .network_attachments()
        .into_iter()
        .flat_map(|attachment| [attachment.clone(), attachment])
        .collect();
    assert!(!duplicated.services[0].matches(&desired.services[0]));
}

#[test]
fn command_and_http_health_checks_have_the_same_runtime_meaning() {
    let mut desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
    let http = piqueld_core::manifest::ValidatedHealthCheck::Http {
        port: 8080,
        path: "/health".into(),
        interval_seconds: 10,
        timeout_seconds: 3,
        start_period_seconds: 0,
        retries: 3,
    };
    desired.services[0].healthcheck = Some(http.clone());
    let mut snapshot = observed(&desired);
    let piqueld_core::manifest::HealthExecution {
        command,
        timing:
            piqueld_core::manifest::HealthTiming {
                interval_seconds,
                timeout_seconds,
                start_period_seconds,
                retries,
            },
    } = http.execution();
    desired.services[0].healthcheck = Some(piqueld_core::manifest::ValidatedHealthCheck::Command {
        command,
        interval_seconds,
        timeout_seconds,
        start_period_seconds,
        retries,
    });
    assert!(snapshot.services[0].matches(&desired.services[0]));
    let request = PlanRequest::Reconcile {
        desired: desired.clone(),
    };
    assert_eq!(
        Plan::from_request(&request, &snapshot).actions,
        [] as [piqueld_core::PlanAction; 0]
    );
    snapshot.services[0].healthcheck = Some(piqueld_core::manifest::ValidatedHealthCheck::Http {
        port: 8081,
        path: "/health".into(),
        interval_seconds,
        timeout_seconds,
        start_period_seconds,
        retries,
    });
    assert!(!snapshot.services[0].matches(&desired.services[0]));
    assert_ne!(
        Plan::from_request(&request, &snapshot).actions,
        [] as [piqueld_core::PlanAction; 0]
    );
}
