//! Plans for converging, cleaning up and deleting an environment.

use piqueld_core::manifest::{RolloutOrder, ValidatedRollout as Rollout};
use piqueld_core::planner::{ActionKind, DiagnosticSeverity, PlanRequest};
use piqueld_core::resource::{Convergence, ObservedApplication, compile_application};
use piqueld_core::{ApplicationId, Plan, parse_toml};

use super::{application, environment, instance, observed, resolutions};

#[test]
fn planner_creates_only_supported_runtime_actions() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &ObservedApplication::default(),
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::EnsureNetwork { .. }))
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::EnsureService { .. }))
    );
    assert!(
        plan.actions
            .iter()
            .all(|action| !matches!(action.kind, ActionKind::ResolveImage { .. }))
    );
}

#[test]
fn plan_shows_effective_rollouts_and_warns_about_overlapping_writers() {
    use piqueld_core::api::ServiceRolloutView;
    use piqueld_core::manifest::RolloutOrderSource;
    let rollout = |app: &piqueld_core::NormalizedApplication| {
        let mut plan = Plan::default();
        plan.warn_rollouts(app, piqueld_core::manifest::Rollback::Manual);
        let [view] = ServiceRolloutView::for_application(app).try_into().unwrap();
        (view, plan)
    };

    // The fixture's only service mounts its volume writable.
    let (derived, plan) = rollout(&application());
    assert_eq!(
        (derived.order, derived.order_source, derived.monitor_seconds),
        (
            RolloutOrder::StopFirst,
            RolloutOrderSource::Derived,
            Rollout::DEFAULT_MONITOR_SECONDS
        )
    );
    assert_eq!(plan.diagnostics, [] as [piqueld_core::PlanDiagnostic; 0]);

    let overlapping = parse_toml(&format!(
        "{}\n[spec.services.rollout]\norder = \"start-first\"\n",
        include_str!("../fixtures/manifests/prebuilt.toml")
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let (explicit, plan) = rollout(&overlapping);
    assert_eq!(
        (explicit.order, explicit.order_source),
        (RolloutOrder::StartFirst, RolloutOrderSource::Explicit)
    );
    let [warning] = plan.diagnostics.as_slice() else {
        panic!("expected one warning, got {:?}", plan.diagnostics);
    };
    assert_eq!(
        (warning.code.as_str(), warning.resource.as_str()),
        (
            piqueld_core::codes::ROLLOUT_START_FIRST_WRITABLE_VOLUME,
            "web"
        )
    );
    assert_eq!(warning.severity, piqueld_core::DiagnosticSeverity::Warning);
    assert!(!plan.is_blocked());
}

/// Automatic rollback warns about routed services without a health check,
/// whose failed rollouts it can't see.
#[test]
fn automatic_rollback_warns_about_routed_services_without_health_checks() {
    use piqueld_core::manifest::Rollback;
    let fixture = include_str!("../fixtures/manifests/prebuilt.toml");
    let app = |extra: &str| {
        parse_toml(&format!("{fixture}{extra}"))
            .unwrap()
            .normalize(ApplicationId::parse("app-notes-01").unwrap())
    };
    let warnings = |app: &piqueld_core::NormalizedApplication, rollback| {
        let mut plan = Plan::default();
        plan.warn_rollouts(app, rollback);
        plan.diagnostics
            .iter()
            .map(|warning| (warning.code.clone(), warning.resource.clone()))
            .collect::<Vec<_>>()
    };
    let route =
        "\n[[spec.routes]]\nhostname = \"notes.example.com\"\nservice = \"web\"\nport = 80\n";
    let routed = app(route);
    assert_eq!(
        warnings(&routed, Rollback::Automatic),
        [(
            piqueld_core::codes::ROLLBACK_WITHOUT_HEALTH_CHECK.to_owned(),
            "web".to_owned()
        )]
    );
    assert_eq!(warnings(&routed, Rollback::Manual), []);
    assert_eq!(warnings(&application(), Rollback::Automatic), []);
    let checked = app(&format!(
        "\n[spec.services.healthcheck]\ntype = \"http\"\nport = 80\n{route}"
    ));
    assert_eq!(warnings(&checked, Rollback::Automatic), []);
}

#[test]
fn converged_services_need_no_work_and_delete_retains_volumes() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let observed = observed(&desired);
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &observed,
    );
    assert!(!plan.has_mutations());
    let deletion = Plan::from_request(
        &PlanRequest::Delete {
            environment_id: desired.id.clone(),
            instance_id: desired.instance_id.clone(),
        },
        &observed,
    );
    assert!(
        deletion
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RetainVolume { .. }))
    );
    assert!(deletion.actions.iter().any(|action| {
        matches!(action.kind, ActionKind::RemoveService { .. }) && action.kind.destructive()
    }));
    assert!(deletion.summary().destructive_count > 0);

    let mut drifted = observed.clone();
    drifted.services[0].runtime_configuration_matches = false;
    let drift_plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &drifted,
    );
    assert!(
        drift_plan
            .actions
            .iter()
            .any(|action| { matches!(action.kind, ActionKind::EnsureService { .. }) })
    );

    let mut foreign = observed;
    foreign.services[0]
        .labels
        .insert("io.piqueld.instance".into(), "other-instance".into());
    let foreign_deletion = Plan::from_request(
        &PlanRequest::Delete {
            environment_id: desired.id,
            instance_id: desired.instance_id,
        },
        &foreign,
    );
    assert!(
        !foreign_deletion
            .actions
            .iter()
            .any(|action| { matches!(action.kind, ActionKind::RemoveService { .. }) })
    );
    assert!(foreign_deletion.is_blocked());
    assert!(foreign_deletion.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == piqueld_core::codes::UNOWNED_NAME_COLLISION && diagnostic.blocking
    }));
}

#[test]
fn obsolete_cleanup_is_gated_and_reports_deferred_diagnostics() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    // Build a fully-converged observation, then add an obsolete owned service
    // while breaking convergence of the wanted service so cleanup stays gated.
    let base = observed(&desired);
    let mut gated = base.clone();
    gated.services[0].convergence = Convergence::Updating;
    let mut stale = gated.services[0].clone();
    stale.name = piqueld_core::docker_resource_name(
        &desired.id,
        piqueld_core::ResourceKind::Service,
        Some("web-stale"),
    );
    stale
        .labels
        .insert("io.piqueld.service".into(), "web-stale".into());
    gated.services.push(stale);
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &gated,
    );
    assert!(
        !plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RemoveService { .. })),
        "cleanup must stay gated while services are not ready"
    );
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "cleanup_deferred"
                && diagnostic.severity == DiagnosticSeverity::Info),
        "{:?}",
        plan.diagnostics
    );

    // Once everything converges, the stale service is removed.
    let ready = observed(&desired);
    let mut converged_with_stale = ready.clone();
    let mut stale_ready = ready.services[0].clone();
    stale_ready.name = piqueld_core::docker_resource_name(
        &desired.id,
        piqueld_core::ResourceKind::Service,
        Some("web-stale"),
    );
    stale_ready
        .labels
        .insert("io.piqueld.service".into(), "web-stale".into());
    converged_with_stale.services.push(stale_ready);
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &converged_with_stale,
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RemoveService { .. }))
    );
}

#[test]
fn failed_convergence_blocks_with_a_service_update_diagnostic() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let mut failed = observed(&desired);
    failed.services[0].convergence = Convergence::Failed;
    let plan = Plan::from_request(&PlanRequest::Reconcile { desired }, &failed);
    assert!(plan.is_blocked());
    let diagnostic = plan
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "service_update_failed")
        .expect("blocking update-failure diagnostic");
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Error);
}

#[test]
fn cleanup_plans_are_stable_across_engine_listing_order() {
    let desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
    let mut snapshot = observed(&desired);
    let mut obsolete_names = Vec::new();
    for logical in ["old-z", "old-a"] {
        let mut service = snapshot.services[0].clone();
        service.name = piqueld_core::docker_resource_name(
            &desired.id,
            piqueld_core::ResourceKind::Service,
            Some(logical),
        );
        service
            .labels
            .insert("io.piqueld.service".into(), logical.into());
        obsolete_names.push(service.name.clone());
        snapshot.services.push(service);
        let mut volume = snapshot.volumes[0].clone();
        volume.name = piqueld_core::docker_resource_name(
            &desired.id,
            piqueld_core::ResourceKind::Volume,
            Some(logical),
        );
        snapshot.volumes.push(volume);
    }
    // Foreign observations must remain diagnostics regardless of their position.
    let mut foreign = snapshot.networks[0].clone();
    foreign.name = "unmanaged-network".into();
    foreign.labels.clear();
    snapshot.networks.push(foreign);
    for request in [
        PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        PlanRequest::Delete {
            environment_id: desired.id.clone(),
            instance_id: desired.instance_id.clone(),
        },
    ] {
        let expected = Plan::from_request(&request, &snapshot);
        for name in &obsolete_names {
            assert!(expected.actions.iter().any(|action| matches!(&action.kind, ActionKind::RemoveService { name: found } if found == name)), "missing cleanup for {name}");
        }
        assert!(
            expected
                .actions
                .iter()
                .any(|action| matches!(action.kind, ActionKind::RetainVolume { .. }))
        );
        for _ in 0..snapshot.services.len() {
            snapshot.services.rotate_left(1);
            snapshot.volumes.reverse();
            snapshot.networks.reverse();
            assert_eq!(Plan::from_request(&request, &snapshot), expected);
        }
        assert!(
            !expected
                .actions
                .iter()
                .any(|action| action.kind.mutates_runtime()
                    && action.kind.resource_name() == "unmanaged-network")
        );
    }
}
