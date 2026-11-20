//! One-shot jobs: their containers, and the services they stop.

use std::collections::BTreeMap;

use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    ObservedApplication, ResolutionSet, ResolvedApplication, compile_application,
    preview_resolution,
};
use piqueld_core::{ApiErrorCode, ApplicationId, Plan, parse_toml};

use super::{environment, instance, observed, resolutions, resolve_all};

#[test]
fn jobs_reuse_their_service_container_under_a_distinct_identity() {
    let app = parse_toml(&format!(
        "{}\n[spec.services.environment]\nRUST_LOG = \"info\"\n\
         [[spec.jobs]]\nname = \"migrate\"\nservice = \"web\"\n\
         command = [\"notes\", \"migrate\"]\nrun = \"before-rollout\"\n",
        include_str!("../fixtures/manifests/prebuilt.toml")
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let (service, job) = (&desired.services[0], &desired.jobs[0]);
    assert!(job.has_valid_identity());
    assert_ne!(job.container.name, service.name);
    assert_eq!(job.container.image, service.image);
    assert_eq!(job.container.environment, service.environment);
    assert_eq!(job.container.mounts, service.mounts);
    // Runs only read volumes unless the job declares exclusive access, also
    // for targets prepared before the setting existed.
    assert!(!service.mounts[0].read_only);
    assert_eq!(job.exclusive_volumes().count(), 0);
    assert!(job.for_operation("operation-1").container.mounts[0].read_only);
    let mut legacy = serde_json::to_value(job).unwrap();
    legacy.as_object_mut().unwrap().remove("volume_access");
    let legacy: piqueld_core::DesiredJob = serde_json::from_value(legacy).unwrap();
    assert!(legacy.for_operation("operation-1").container.mounts[0].read_only);
    assert_eq!(job.container.networks, service.networks);
    assert_eq!(job.container.command, ["notes", "migrate"]);
    assert!(job.container.healthcheck.is_none());
    assert!(!job.container.labels.contains_key("io.piqueld.service"));
    assert_eq!(
        job.container
            .labels
            .get("io.piqueld.job")
            .map(String::as_str),
        Some("migrate")
    );
    assert_eq!(
        piqueld_core::OwnershipState::for_resource(
            &job.container.labels,
            &instance(),
            &environment(),
            piqueld_core::ResourceKind::Job,
            job.container.name.as_str(),
        ),
        piqueld_core::OwnershipState::Owned
    );
    // A run is labelled with the operation that starts it.
    let operation_label = piqueld_core::resource::JOB_OPERATION_LABEL;
    assert!(!job.container.labels.contains_key(operation_label));
    let run = job.for_operation("operation-1");
    assert_eq!(run.operation(), "operation-1");
    assert_eq!(
        run.container
            .labels
            .get(operation_label)
            .map(String::as_str),
        Some("operation-1")
    );
    assert!(run.has_valid_identity());
}

#[test]
fn jobs_add_their_environment_and_secrets_to_their_container_only() {
    let app = parse_toml(&format!(
        "{}\n[spec.services.environment]\nRUST_LOG = \"info\"\n\
         [[spec.jobs]]\nname = \"migrate\"\nservice = \"web\"\n\
         command = [\"notes\", \"migrate\"]\nrun = \"before-rollout\"\n\
         [spec.jobs.environment]\nOWNER_PASSWORD_FILE = \"/run/secrets/owner\"\n\
         [[spec.jobs.secrets]]\nname = \"owner-password\"\ntarget = \"/run/secrets/owner\"\n",
        include_str!("../fixtures/manifests/prebuilt.toml")
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let errors = compile_application(&app, &environment(), instance(), &resolutions()).unwrap_err();
    assert_eq!(
        errors
            .iter()
            .map(|error| (error.code.as_str(), error.resource.as_str()))
            .collect::<Vec<_>>(),
        [(ApiErrorCode::SecretUnresolved.as_str(), "migrate")]
    );

    let mut pinned = resolutions();
    pinned
        .secret_names
        .insert("owner-password".into(), "owner-password-v1".into());
    let desired = compile_application(&app, &environment(), instance(), &pinned).unwrap();
    let (service, job) = (&desired.services[0], &desired.jobs[0]);
    assert_eq!(
        service.environment,
        BTreeMap::from([("RUST_LOG".into(), "info".into())])
    );
    assert_eq!(service.secrets, []);
    assert_eq!(
        job.container.environment,
        BTreeMap::from([
            ("OWNER_PASSWORD_FILE".into(), "/run/secrets/owner".into()),
            ("RUST_LOG".into(), "info".into()),
        ])
    );
    assert_eq!(
        job.container.secrets,
        [piqueld_core::resource::SecretFile {
            secret_name: "owner-password-v1".into(),
            target: "/run/secrets/owner".into(),
        }]
    );
}

/// The fixture with a migration of `web`, which waits for `db`, as
/// `configure` sets it up.
fn migration(
    configure: impl FnOnce(&mut piqueld_core::manifest::Job),
) -> piqueld_core::NormalizedApplication {
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let mut manifest = parse_toml(&format!(
        "{}[[spec.services]]\nname = \"db\"\n[spec.services.source]\n\
         type = \"image\"\nimage = \"ghcr.io/example/notes:1.4.0\"\n\
         [[spec.jobs]]\nname = \"migrate\"\nservice = \"web\"\n\
         command = [\"notes\", \"migrate\"]\nrun = \"before-rollout\"\n",
        include_str!("../fixtures/manifests/prebuilt.toml")
    ))
    .unwrap()
    .normalize(id.clone())
    .to_manifest();
    configure(&mut manifest.spec.jobs[0]);
    for service in &mut manifest.spec.services {
        if service.name == "web" {
            service.depends_on = vec!["db".into()];
        }
    }
    manifest.validate().unwrap().normalize(id)
}

/// The fixture's exclusive migration of `web`.
fn exclusive_migration() -> piqueld_core::NormalizedApplication {
    migration(|job| job.volume_access = piqueld_core::manifest::JobVolumeAccess::Exclusive)
}

/// The fixture's `web` service of `desired`.
fn web(desired: &ResolvedApplication) -> &piqueld_core::DesiredService {
    desired
        .services
        .iter()
        .find(|service| service.logical_name.as_str() == "web")
        .unwrap()
}

#[test]
fn exclusive_jobs_stop_volume_users_after_dependencies_and_repair_keeps_them_stopped() {
    let desired = resolve_all(&exclusive_migration());
    let job = &desired.jobs[0];
    assert_eq!(
        job.exclusive_volumes().collect::<Vec<_>>(),
        [&web(&desired).mounts[0].volume_name]
    );
    assert!(!job.for_operation("operation-1").container.mounts[0].read_only);
    assert_job_stops_web(&desired);
}

#[test]
fn jobs_stop_their_own_service_after_dependencies_and_repair_keeps_it_stopped() {
    let desired = resolve_all(&migration(|job| job.stop_service = true));
    let job = &desired.jobs[0];
    assert_eq!(job.exclusive_volumes().count(), 0);
    assert_eq!(job.stopped_service().as_ref(), Some(&web(&desired).name));
    assert_job_stops_web(&desired);
}

/// Preparing the fixture's migration starts `db`, then stops `web`, and
/// repair of the previous target leaves `web` stopped.
fn assert_job_stops_web(desired: &ResolvedApplication) {
    let job = &desired.jobs[0];
    let web = web(desired);
    let plan_for = |observed: &ObservedApplication| {
        Plan::from_request(
            &PlanRequest::Reconcile {
                desired: desired.clone(),
            },
            observed,
        )
    };
    let next = |observed: &ObservedApplication| {
        plan_for(observed)
            .next_job_preparation(desired, job, observed)
            .map(|action| action.kind.to_string())
    };

    // The dependency starts first, then the running web stops.
    let mut running = observed(desired);
    running
        .services
        .retain(|service| service.name == web.name.as_str());
    assert_eq!(next(&running).as_deref(), Some("ENSURE SERVICE db"));
    let mut ready = observed(desired);
    assert_eq!(next(&ready), Some(format!("REMOVE SERVICE {}", web.name)));
    ready
        .services
        .retain(|service| service.name != web.name.as_str());
    assert_eq!(next(&ready), None);

    // Repair of the previous target must not restart web while the job may
    // still run or has changed what web uses.
    let mut repair = plan_for(&ready);
    assert_eq!(
        repair.next_repair().unwrap().kind.to_string(),
        "ENSURE SERVICE web"
    );
    repair.preserve_job_prerequisites(desired, &ready);
    assert!(repair.next_repair().is_none());
    assert!(
        repair
            .actions
            .iter()
            .all(|action| !action.kind.to_string().contains("web")
                && !action.kind.to_string().contains(web.name.as_str()))
    );
}

#[test]
fn previews_list_jobs_after_their_dependencies_and_warn_about_stopped_services() {
    let app = exclusive_migration();
    let desired = resolve_all(&app);
    let mut plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved: Vec::new(),
            desired: Some(desired),
        },
        &ObservedApplication::default(),
    );
    plan.add_jobs(&app, &environment());
    let names = plan
        .actions
        .iter()
        .map(|action| action.kind.to_string())
        .collect::<Vec<_>>();
    let position = |prefix: &str| {
        names
            .iter()
            .position(|name| name.starts_with(prefix))
            .unwrap()
    };
    assert!(position("ENSURE SERVICE db") < position("RUN JOB migrate"));
    assert!(position("WAIT SERVICE") < position("RUN JOB migrate"));
    assert!(position("RUN JOB migrate") < position("ENSURE SERVICE web"));
    assert_eq!(
        plan.diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.resource.as_str()))
            .collect::<Vec<_>>(),
        [(
            piqueld_core::ApiErrorCode::JobStopsServices.as_str(),
            "migrate"
        )]
    );

    // Unresolved sources leave only resolution actions, which jobs follow.
    let mut plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved: preview_resolution(&app, &ResolutionSet::default()),
            desired: None,
        },
        &ObservedApplication::default(),
    );
    plan.add_jobs(&app, &environment());
    assert_eq!(
        plan.actions.last().map(|action| action.kind.to_string()),
        Some("RUN JOB migrate".into())
    );

    // A job stopping its own service warns about that service alone.
    let app = migration(|job| job.stop_service = true);
    let mut plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved: Vec::new(),
            desired: Some(resolve_all(&app)),
        },
        &ObservedApplication::default(),
    );
    plan.add_jobs(&app, &environment());
    assert_eq!(
        plan.diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.message.as_str()))
            .collect::<Vec<_>>(),
        [(
            piqueld_core::ApiErrorCode::JobStopsServices.as_str(),
            "job migrate stops web before it runs, until the rollout"
        )]
    );
}
