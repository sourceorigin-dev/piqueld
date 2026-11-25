use super::super::HealthCheck;
use super::*;
use bollard::models::{ServiceSpecRollbackConfig, ServiceSpecUpdateConfigOrderEnum};
use piqueld_core::manifest::{
    RolloutOrder, ValidatedResourceLimits as ResourceLimits, ValidatedRollout as Rollout,
};
use piqueld_core::resource::{DesiredMount, DesiredService, ResolvedSource};
use std::collections::BTreeMap;

/// Specification authored for a service attached to its private network.
fn authored() -> ServiceSpec {
    BollardDocker::service_spec(&desired(Vec::new()), "local-node").expect("authored specification")
}

/// Desired service attached to its private network and mounting `mounts`.
fn desired(mounts: Vec<DesiredMount>) -> DesiredService {
    let image = format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64));
    let application =
        piqueld_core::EnvironmentId::parse("app-policy").expect("the environment id parses");
    DesiredService {
        link_aliases: BTreeMap::new(),
        secrets: Vec::new(),
        files: Vec::new(),
        logical_name: piqueld_core::ServiceName::parse("web").expect("the service name parses"),
        name: piqueld_core::DockerServiceName::parse("app-policy-web")
            .expect("the docker service name parses"),
        source: ResolvedSource::Image {
            requested: piqueld_core::ImageReference::parse("ghcr.io/example/notes:1.4.0")
                .expect("the image reference parses"),
            digest_reference: piqueld_core::RepositoryDigest::parse(image.clone())
                .expect("the repository digest parses"),
        },
        image: piqueld_core::ImmutableImage::parse(image).expect("the image parses"),
        replicas: 2,
        environment: BTreeMap::from([("NOTES_PORT".into(), "8080".into())]),
        command: vec!["/bin/notes".into()],
        arguments: vec!["--listen".into(), "8080".into()],
        mounts,
        healthcheck: None,
        resources: Some(ResourceLimits {
            cpu_millis: Some(250),
            memory_bytes: Some(1024),
        }),
        networks: vec![piqueld_core::DockerNetworkName::for_application(
            &application,
        )],
        labels: piqueld_core::Ownership {
            instance_id: piqueld_core::InstanceId::parse("instance")
                .expect("the instance id parses"),
            environment_id: application,
            service: Some(
                piqueld_core::ServiceName::parse("web").expect("the service name parses"),
            ),
            spec_hash: format!("sha256:{}", "b".repeat(64)),
        }
        .labels(),
        depends_on: Vec::new(),
        rollout: Rollout::default(),
        security: ServiceSecurity::default(),
        receives_telemetry: false,
        telemetry: None,
    }
}

#[test]
fn authored_rollout_is_observed_back() {
    let mount = |read_only| DesiredMount {
        volume_name: piqueld_core::DockerVolumeName::parse("app-policy-data")
            .expect("the docker volume name parses"),
        target: "/data".into(),
        read_only,
    };
    let observed = |desired: &DesiredService| {
        let spec =
            BollardDocker::service_spec(desired, "local-node").expect("the service spec builds");
        assert!(ServiceRuntimePolicy::matches(&spec, "local-node"));
        BollardDocker::observed_rollout(&spec)
    };

    let read_only = desired(vec![mount(true)]);
    assert_eq!(observed(&read_only), Some(read_only.rollout_policy()));
    let mut writable = desired(vec![mount(true), mount(false)]);
    assert_eq!(
        observed(&writable).map(|rollout| rollout.order),
        Some(RolloutOrder::StopFirst)
    );
    writable.rollout = Rollout {
        order: Some(RolloutOrder::StartFirst),
        monitor_seconds: Some(5),
    };
    assert_eq!(observed(&writable), Some(writable.rollout_policy()));

    let mut spec =
        BollardDocker::service_spec(&writable, "local-node").expect("the service spec builds");
    let update = spec.update_config.as_mut().expect("the field is set");
    update.monitor = Some(1_500_000_000);
    assert_eq!(BollardDocker::observed_rollout(&spec), None);
    let update = spec.update_config.as_mut().expect("the field is set");
    update.monitor = Some(5_000_000_000);
    update.order = Some(ServiceSpecUpdateConfigOrderEnum::EMPTY);
    assert_eq!(BollardDocker::observed_rollout(&spec), None);
}

/// Each security setting is authored, accepted by the policy and
/// observed back; values it can't express are drift.
#[test]
fn authored_security_is_observed_back() {
    let mut hardened = desired(Vec::new());
    hardened.security = ServiceSecurity {
        user: Some("1000:1000".parse().expect("the user parses")),
        read_only_root: true,
        drop_capabilities: true,
        no_new_privileges: true,
    };
    let mut spec =
        BollardDocker::service_spec(&hardened, "local-node").expect("the service spec builds");
    assert!(ServiceRuntimePolicy::matches(&spec, "local-node"));
    let container = spec
        .task_template
        .as_mut()
        .and_then(|task| task.container_spec.as_mut())
        .expect("the container spec is set");
    assert_eq!(
        BollardDocker::observed_security(container),
        hardened.security
    );
    assert_eq!(
        BollardDocker::observed_security(
            BollardDocker::service_spec(&desired(Vec::new()), "local-node")
                .expect("the service spec builds")
                .task_template
                .expect("the service has a task template")
                .container_spec
                .as_ref()
                .expect("the field is set")
        ),
        ServiceSecurity::default()
    );
    let drifted = |change: fn(&mut TaskSpecContainerSpec)| {
        let mut spec = spec.clone();
        change(
            spec.task_template
                .as_mut()
                .and_then(|task| task.container_spec.as_mut())
                .expect("the container spec is set"),
        );
        !ServiceRuntimePolicy::matches(&spec, "local-node")
    };
    assert!(drifted(|container| container.user = Some("app".into())));
    assert!(drifted(|container| container.read_only = Some(false)));
    assert!(drifted(|container| {
        container.capability_drop = Some(vec!["NET_RAW".into()]);
    }));
    assert!(drifted(|container| {
        container
            .privileges
            .as_mut()
            .expect("the field is set")
            .app_armor = Some(bollard::models::TaskSpecContainerSpecPrivilegesAppArmor::default());
    }));
}

/// A bind mount at the telemetry target on a service that receives no
/// telemetry is observed as telemetry, so it is drift, and keeps the
/// container's user out of its security settings.
#[test]
fn a_telemetry_mount_without_a_receiver_is_observed_as_telemetry() {
    let plain = desired(Vec::new());
    let mut spec =
        BollardDocker::service_spec(&plain, "local-node").expect("the service spec builds");
    let container = spec
        .task_template
        .as_mut()
        .and_then(|task| task.container_spec.as_mut())
        .expect("the container spec is set");
    container
        .mounts
        .get_or_insert_default()
        .push(bollard::models::Mount {
            target: Some(piqueld_core::resource::TELEMETRY_TARGET.into()),
            source: Some("/srv/telemetry".into()),
            typ: Some(MountTypeEnum::BIND),
            ..Default::default()
        });
    let observed = BollardDocker::observed_telemetry(container);
    assert_eq!(
        observed,
        Some(piqueld_core::resource::TelemetryMount {
            source: "/srv/telemetry".into(),
            user: String::new(),
        })
    );
    assert_ne!(observed, plain.telemetry);
    container.user = Some("1000".into());
    assert_eq!(BollardDocker::observed_security(container).user, None);
    assert_eq!(
        BollardDocker::observed_telemetry(container).map(|telemetry| telemetry.user),
        Some("1000".into())
    );
}

/// Swarm would expand `{{` as a Go template, so it is escaped when
/// authored and read back as written.
#[test]
fn template_text_is_authored_literally_and_observed_back() {
    let text = r#"${{ vars.x }} {{.Service.Name}} {{{ {{"{{"}}"#;
    let mut desired = desired(vec![DesiredMount {
        volume_name: piqueld_core::DockerVolumeName::parse("app-policy-data")
            .expect("the docker volume name parses"),
        target: "/data/{{.Task.Slot}}".into(),
        read_only: false,
    }]);
    desired.environment.insert("TEXT".into(), text.into());
    let spec =
        BollardDocker::service_spec(&desired, "local-node").expect("the service spec builds");
    let container = spec
        .task_template
        .as_ref()
        .expect("the field is set")
        .container_spec
        .as_ref();
    assert!(
        container.expect("the service has a container").env.as_ref().expect("the container has an environment").contains(
            &r#"TEXT=${{"{{"}} vars.x }} {{"{{"}}.Service.Name}} {{"{{"}}{ {{"{{"}}"{{"{{"}}"}}"#
                .into()
        )
    );
    let observed = BollardDocker::observe_service(&spec, "local-node", Vec::new(), None)
        .expect("observed service");
    assert_eq!(observed.environment, desired.environment);
    assert_eq!(observed.mounts[0].target, "/data/{{.Task.Slot}}");
    assert!(observed.matches(&desired));

    let unescaped = |edit: fn(&mut TaskSpecContainerSpec)| {
        let mut spec = spec.clone();
        edit(
            spec.task_template
                .as_mut()
                .expect("the field is set")
                .container_spec
                .as_mut()
                .expect("the field is set"),
        );
        BollardDocker::observe_service(&spec, "local-node", Vec::new(), None)
            .expect("observed service")
            .drift(&desired)
    };
    assert_eq!(
        unescaped(|container| container.env = Some(vec!["TEXT={{.Service.Name}}".into()])),
        ["environment", "runtime_policy"]
    );
    assert_eq!(
        unescaped(|container| {
            container.mounts.as_mut().expect("the field is set")[0].target =
                Some("/data/{{.Task.Slot}}".into());
        }),
        ["runtime_policy"]
    );
}

#[test]
fn logical_name_is_the_only_accepted_alias() {
    let mut spec = authored();
    assert!(ServiceRuntimePolicy::matches(&spec, "local-node"));
    let network = &mut spec
        .task_template
        .as_mut()
        .expect("the field is set")
        .networks
        .as_mut()
        .expect("the field is set")[0];
    assert_eq!(network.aliases, Some(vec!["web".into()]));
    network
        .aliases
        .as_mut()
        .expect("the field is set")
        .push("other".into());
    assert!(!ServiceRuntimePolicy::matches(&spec, "local-node"));
}

/// Including a start period and retries other than Docker's defaults.
#[test]
fn generated_http_health_check_round_trips_through_observation() {
    let health_check = HealthCheck::Http {
        port: 8080,
        path: "/health".into(),
        interval_seconds: 10,
        timeout_seconds: 3,
        start_period_seconds: 30,
        retries: 5,
    };
    let config = BollardDocker::health_config(&health_check);

    assert_eq!(BollardDocker::observed_health(&config), Some(health_check));
    assert!(ServiceRuntimePolicy::supported_health_config(&config));
}

/// The probe vector piqueld authors for `Http { port: 8080, path: "/health", .. }`.
fn probe_vector(arguments: &[&str]) -> Vec<String> {
    std::iter::once(piqueld_probe::CONTAINER_PATH)
        .chain(arguments.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn command_health_check_accepts_equivalent_http_vector() {
    let health_check = HealthCheck::Command {
        command: probe_vector(&[
            "http",
            "--port",
            "8080",
            "--path",
            "/health",
            "--timeout",
            "3",
        ]),
        interval_seconds: 10,
        timeout_seconds: 3,
        start_period_seconds: 0,
        retries: 3,
    };

    let config = BollardDocker::health_config(&health_check);
    let observed = BollardDocker::observed_health(&config).expect("the health is observed");
    assert!(matches!(observed, HealthCheck::Http { port: 8080, .. }));
    assert_eq!(observed.execution(), health_check.execution());
    assert!(ServiceRuntimePolicy::supported_health_config(&config));
}

/// Near matches, including the `wget` vector earlier daemons authored for
/// HTTP checks, stay commands; HTTP services deployed before the probe
/// therefore drift and roll onto it.
#[test]
fn near_http_command_health_checks_round_trip_as_commands() {
    for command in [
        probe_vector(&[
            "http",
            "--path",
            "/health",
            "--port",
            "8080",
            "--timeout",
            "3",
        ]),
        probe_vector(&[
            "http",
            "--port",
            "8080",
            "--path",
            "/health",
            "--timeout",
            "5",
        ]),
        [
            "wget",
            "-q",
            "-T",
            "3",
            "-O",
            "/dev/null",
            "http://127.0.0.1:8080/health",
        ]
        .map(String::from)
        .into(),
    ] {
        let health_check = HealthCheck::Command {
            command,
            interval_seconds: 10,
            timeout_seconds: 3,
            start_period_seconds: 0,
            retries: 3,
        };

        let config = BollardDocker::health_config(&health_check);
        assert_eq!(
            BollardDocker::observed_health(&config).as_ref(),
            Some(&health_check)
        );
        assert!(ServiceRuntimePolicy::supported_health_config(&config));
    }
}

#[test]
fn policy_verifies_exactly_the_authored_fields() {
    let authored = authored();
    assert!(ServiceRuntimePolicy::matches(&authored, "local-node"));
    assert!(!ServiceRuntimePolicy::matches(&authored, "another-node"));
    let mut unpinned = authored.clone();
    unpinned
        .task_template
        .as_mut()
        .expect("the field is set")
        .placement = None;
    assert!(!ServiceRuntimePolicy::matches(&unpinned, "local-node"));

    let mut echoed = authored.clone();
    let task = echoed.task_template.as_mut().expect("task");
    task.resources.as_mut().expect("resources").reservations =
        Some(bollard::models::ResourceObject::default());
    task.runtime = Some("container".into());
    task.force_update = Some(0);
    task.placement.as_mut().expect("placement").max_replicas = Some(0);
    task.restart_policy.as_mut().expect("restart").max_attempts = Some(0);
    task.container_spec.as_mut().expect("container").dns_config =
        Some(bollard::models::TaskSpecContainerSpecDnsConfig::default());
    echoed.rollback_config = Some(ServiceSpecRollbackConfig {
        parallelism: Some(1),
        failure_action: Some(bollard::models::ServiceSpecRollbackConfigFailureActionEnum::PAUSE),
        monitor: Some(5 * super::super::NANOSECONDS_PER_SECOND),
        max_failure_ratio: Some(0.0),
        order: Some(bollard::models::ServiceSpecRollbackConfigOrderEnum::STOP_FIRST),
        ..Default::default()
    });
    assert!(ServiceRuntimePolicy::matches(&echoed, "local-node"));

    echoed.rollback_config = Some(ServiceSpecRollbackConfig::default());
    if let Some(container) = echoed
        .task_template
        .as_mut()
        .and_then(|task| task.container_spec.as_mut())
    {
        container.stop_grace_period = Some(12_345);
        container.hostname = Some("echoed".into());
    }
    assert!(!ServiceRuntimePolicy::matches(&echoed, "local-node"));

    let mut runtime = authored.clone();
    runtime
        .task_template
        .as_mut()
        .expect("the field is set")
        .runtime = Some("custom-runtime".into());
    assert!(!ServiceRuntimePolicy::matches(&runtime, "local-node"));

    let mut drifted = authored.clone();
    if let Some(update) = drifted.update_config.as_mut() {
        update.parallelism = Some(4);
    }
    assert!(!ServiceRuntimePolicy::matches(&drifted, "local-node"));

    let mut injected_capability = authored.clone();
    if let Some(container) = injected_capability
        .task_template
        .as_mut()
        .and_then(|task| task.container_spec.as_mut())
    {
        container.capability_add = Some(vec!["NET_ADMIN".into()]);
    }
    assert!(!ServiceRuntimePolicy::matches(
        &injected_capability,
        "local-node"
    ));

    let mut injected_user = authored;
    if let Some(container) = injected_user
        .task_template
        .as_mut()
        .and_then(|task| task.container_spec.as_mut())
    {
        container.user = Some("root".into());
    }
    assert!(!ServiceRuntimePolicy::matches(&injected_user, "local-node"));
}
