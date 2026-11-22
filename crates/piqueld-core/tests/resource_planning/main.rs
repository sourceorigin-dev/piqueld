//! Focused resolution and supported-runtime planning coverage.

use std::collections::BTreeMap;

use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    Convergence, ObservedApplication, ObservedNetwork, ObservedService, ObservedTask,
    ObservedVolume, ResolutionSet, ResolvedApplication, ResolvedSource, TaskState,
    compile_application,
};
use piqueld_core::{ApplicationId, EnvironmentId, InstanceId, Plan, parse_toml};

mod dependencies;
mod drift;
mod jobs;
mod links;
mod ownership;
mod planning;
mod resolution;

fn application() -> piqueld_core::NormalizedApplication {
    parse_toml(include_str!("../fixtures/manifests/prebuilt.toml"))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap())
}

fn environment() -> EnvironmentId {
    EnvironmentId::parse("app-notes-01").unwrap()
}

fn instance() -> InstanceId {
    InstanceId::parse("instance-test").unwrap()
}

fn resolutions() -> ResolutionSet {
    ResolutionSet {
        secret_names: BTreeMap::new(),
        sources: [(
            piqueld_core::ServiceName::parse("web").unwrap(),
            ResolvedSource::Image {
                requested: piqueld_core::ImageReference::parse("ghcr.io/example/notes:1.4.0")
                    .unwrap(),
                digest_reference: piqueld_core::RepositoryDigest::parse(format!(
                    "ghcr.io/example/notes@sha256:{}",
                    "a".repeat(64)
                ))
                .unwrap(),
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn observed(desired: &ResolvedApplication) -> ObservedApplication {
    ObservedApplication {
        networks: desired
            .networks
            .iter()
            .map(|network| ObservedNetwork {
                name: network.name.to_string(),
                runtime_configuration_matches: true,
                labels: network.labels.clone(),
                in_use: false,
            })
            .collect(),
        volumes: desired
            .volumes
            .iter()
            .map(|volume| ObservedVolume {
                name: volume.name.to_string(),
                runtime_configuration_matches: true,
                labels: volume.labels.clone(),
            })
            .collect(),
        services: desired
            .services
            .iter()
            .map(|service| ObservedService {
                secrets: Vec::new(),
                files: Vec::new(),
                name: service.name.to_string(),
                image: service.image.to_string(),
                replicas: service.replicas,
                environment: service.environment.clone(),
                command: service.command.clone(),
                arguments: service.arguments.clone(),
                mounts: service
                    .mounts
                    .iter()
                    .map(piqueld_core::resource::ObservedMount::from)
                    .collect(),
                healthcheck: service.healthcheck.clone(),
                healthcheck_configured: service.healthcheck.is_some(),
                resources: service.resources.clone(),
                networks: service.network_attachments(),
                labels: service.labels.clone(),
                rollout: Some(service.rollout_policy()),
                security: service.security,
                telemetry: service.telemetry.clone(),
                runtime_configuration_matches: true,
                tasks: vec![ObservedTask {
                    state: TaskState::Running,
                    healthy: Some(true),
                    desired_running: true,
                    diagnostic: None,
                    ..ObservedTask::default()
                }],
                convergence: Convergence::Converged,
            })
            .collect(),
        linked_networks: Vec::new(),
    }
}

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}

fn labels_for(instance: &InstanceId, application: &EnvironmentId) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), application.to_string()),
        ("io.piqueld.spec-hash".into(), digest()),
    ])
}

fn reconcile_plan(desired: &ResolvedApplication, observed: &ObservedApplication) -> Plan {
    Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        observed,
    )
}

/// Resolves every service of `app` to the fixture image.
fn resolve_all(app: &piqueld_core::NormalizedApplication) -> ResolvedApplication {
    let source = resolutions().sources.into_values().next().unwrap();
    let resolutions = ResolutionSet {
        secret_names: BTreeMap::new(),
        sources: app
            .spec()
            .services
            .iter()
            .map(|service| (service.name.clone(), source.clone()))
            .collect(),
    };
    compile_application(app, &environment(), instance(), &resolutions).unwrap()
}
