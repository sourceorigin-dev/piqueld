//! Canonical names and ownership labels of desired and observed resources.

use std::collections::BTreeMap;

use piqueld_core::manifest::ValidatedRollout as Rollout;
use piqueld_core::planner::{ActionKind, PlanRequest};
use piqueld_core::resource::{
    DesiredNetwork, DesiredService, DesiredVolume, NetworkAttachment, ObservedApplication,
    OwnershipState, Sha256Digest, compile_application,
};
use piqueld_core::{DockerNetworkName, EnvironmentId, InstanceId, Plan};

use super::{application, digest, environment, instance, labels_for, observed, resolutions};

#[test]
fn desired_identity_matrices_reject_non_canonical_resources() {
    let id = EnvironmentId::parse("app-notes-01").expect("the environment id is valid");
    let instance = instance();
    let network = DesiredNetwork {
        name: DockerNetworkName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Network,
            None,
        ))
        .expect("the docker network name is valid"),
        labels: labels_for(&instance, &id),
    };
    assert!(network.has_valid_identity());

    let mut mislabeled = network.clone();
    mislabeled
        .labels
        .insert("io.piqueld.service".into(), "web".into());
    assert!(!mislabeled.has_valid_identity());

    let mut renamed = network.clone();
    renamed.name = DockerNetworkName::parse(format!("{}x", renamed.name))
        .expect("the docker network name is valid");
    assert!(!renamed.has_valid_identity());

    let volume = DesiredVolume {
        logical_name: piqueld_core::VolumeName::parse("data").expect("the volume name is valid"),
        name: piqueld_core::DockerVolumeName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Volume,
            Some("data"),
        ))
        .expect("the docker volume name is valid"),
        labels: labels_for(&instance, &id),
    };
    assert!(volume.has_valid_identity());

    let resolved = resolutions()
        .sources
        .get("web")
        .expect("the entry exists")
        .clone();
    let service = DesiredService {
        link_aliases: BTreeMap::new(),
        secrets: Vec::new(),
        files: Vec::new(),
        logical_name: piqueld_core::ServiceName::parse("web").expect("the service name is valid"),
        name: piqueld_core::DockerServiceName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Service,
            Some("web"),
        ))
        .expect("the docker service name is valid"),
        source: resolved,
        image: piqueld_core::ImmutableImage::parse(format!("ghcr.io/example/notes@{}", digest()))
            .expect("the immutable image is valid"),
        replicas: 1,
        environment: BTreeMap::default(),
        command: Vec::new(),
        arguments: Vec::new(),
        mounts: Vec::new(),
        healthcheck: None,
        resources: None,
        depends_on: Vec::new(),
        rollout: Rollout::default(),
        security: piqueld_core::manifest::ServiceSecurity::default(),
        receives_telemetry: false,
        telemetry: None,
        networks: vec![
            DockerNetworkName::parse(piqueld_core::docker_resource_name(
                &id,
                piqueld_core::ResourceKind::Network,
                None,
            ))
            .expect("the docker network name is valid"),
        ],
        labels: {
            let mut labels = labels_for(&instance, &id);
            labels.insert("io.piqueld.service".into(), "web".into());
            labels
        },
    };
    assert!(service.has_valid_identity());

    let mut wrong_service_label = service.clone();
    wrong_service_label
        .labels
        .insert("io.piqueld.service".into(), "other".into());
    assert!(!wrong_service_label.has_valid_identity());
}

#[test]
fn ownership_states_classify_labels() {
    let id = EnvironmentId::parse("app-notes-01").expect("the environment id is valid");
    let instance = instance();

    assert_eq!(
        OwnershipState::from_labels(&labels_for(&instance, &id), &instance, &id),
        OwnershipState::Owned
    );

    let foreign = OwnershipState::from_labels(
        &labels_for(
            &InstanceId::parse("other-instance").expect("the instance id is valid"),
            &id,
        ),
        &instance,
        &id,
    );
    assert_eq!(foreign, OwnershipState::Foreign);

    for mutation in [
        ("io.piqueld.managed", "false"),
        ("io.piqueld.spec-hash", "sha256:nothex"),
    ] {
        let mut invalid = labels_for(&instance, &id);
        invalid.insert(mutation.0.into(), mutation.1.into());
        assert_eq!(
            OwnershipState::from_labels(&invalid, &instance, &id),
            OwnershipState::Invalid
        );
    }

    let missing_hash = labels_for(&instance, &id);
    let missing_hash = missing_hash
        .into_iter()
        .filter(|(key, _)| key != "io.piqueld.spec-hash")
        .collect();
    assert_eq!(
        OwnershipState::from_labels(&missing_hash, &instance, &id),
        OwnershipState::Invalid
    );
}

#[test]
fn logical_name_is_an_alias_only_on_the_private_network() {
    let app = application();
    let mut desired = compile_application(&app, &environment(), instance(), &resolutions())
        .expect("the application compiles");
    let ingress = DockerNetworkName::for_ingress(&environment());
    desired.services[0].networks.push(ingress.clone());
    assert_eq!(
        desired.services[0].network_attachments(),
        vec![
            NetworkAttachment {
                network: DockerNetworkName::for_application(&environment()).to_string(),
                aliases: vec!["web".into()],
            },
            NetworkAttachment {
                network: ingress.to_string(),
                aliases: Vec::new(),
            },
        ]
    );

    let mut unaliased = observed(&desired);
    for attachment in &mut unaliased.services[0].networks {
        attachment.aliases.clear();
    }
    assert!(!unaliased.services[0].matches(&desired.services[0]));

    let mut exposed = observed(&desired);
    exposed.services[0].networks[1].aliases = vec!["web".into()];
    assert!(!exposed.services[0].matches(&desired.services[0]));
}

#[test]
fn compiled_ownership_carries_the_spec_hash_label() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions())
        .expect("the application compiles");
    let hash = Sha256Digest::parse(desired.spec_hash.clone()).expect("the sha256 digest is valid");
    for network in &desired.networks {
        assert_eq!(
            network
                .labels
                .get("io.piqueld.spec-hash")
                .map(String::as_str),
            Some(hash.as_str())
        );
    }
}

#[test]
fn desired_names_are_checked_while_engine_observations_remain_permissive() {
    let desired = compile_application(&application(), &environment(), instance(), &resolutions())
        .expect("the application compiles");
    let mut wire = serde_json::to_value(&desired).expect("the value serializes");
    wire["services"][0]["name"] = "Foreign_Service".into();
    serde_json::from_value::<piqueld_core::ResolvedApplication>(wire)
        .expect_err("`serde_json::from_value` should be rejected");

    let mut snapshot = serde_json::to_value(observed(&desired)).expect("the value serializes");
    snapshot["services"][0]["name"] = "Foreign_Service".into();
    snapshot["services"][0]["mounts"] = serde_json::json!([
        {"volume_name":"Foreign.Volume", "target":"/data", "read_only":false}
    ]);
    let observation: ObservedApplication =
        serde_json::from_value(snapshot).expect("the input deserializes");
    assert_eq!(observation.services[0].name, "Foreign_Service");
    assert_eq!(
        observation.services[0].mounts[0].volume_name,
        "Foreign.Volume"
    );
    assert!(!observation.services[0].is_owned_by(&desired.instance_id, &desired.id));
}

#[test]
fn planner_rejects_wrong_resource_roles_before_use_or_deletion() {
    use piqueld_core::resource::SERVICE_LABEL;
    use piqueld_core::{ResourceKind, docker_resource_name};
    let desired = compile_application(&application(), &environment(), instance(), &resolutions())
        .expect("the application compiles");
    for role in [
        ResourceKind::Network,
        ResourceKind::Volume,
        ResourceKind::Service,
    ] {
        let mut snapshot = observed(&desired);
        match role {
            ResourceKind::Network => {
                snapshot.networks[0]
                    .labels
                    .insert(SERVICE_LABEL.into(), "web".into());
            }
            ResourceKind::Volume => {
                snapshot.volumes[0]
                    .labels
                    .insert(SERVICE_LABEL.into(), "web".into());
            }
            ResourceKind::Service => {
                let service = &mut snapshot.services[0];
                service
                    .labels
                    .insert(SERVICE_LABEL.into(), "INVALID".into());
                service.name = docker_resource_name(&desired.id, role, Some("INVALID"));
            }
            ResourceKind::Job | ResourceKind::File => {
                panic!("observation never reports jobs or configs")
            }
        }
        let deletion = Plan::from_request(
            &PlanRequest::Delete {
                environment_id: desired.id.clone(),
                instance_id: instance(),
            },
            &snapshot,
        );
        assert!(deletion.is_blocked(), "{role:?}");
        if role != ResourceKind::Service {
            let plan = Plan::from_request(
                &PlanRequest::Reconcile {
                    desired: desired.clone(),
                },
                &snapshot,
            );
            assert!(plan.is_blocked(), "{role:?}");
            assert!(
                !plan
                    .actions
                    .iter()
                    .any(|action| matches!(action.kind, ActionKind::EnsureService { .. }))
            );
        }
    }
    let mut snapshot = observed(&desired);
    snapshot.networks[0].name.push_str("-other");
    let deletion = Plan::from_request(
        &PlanRequest::Delete {
            environment_id: desired.id.clone(),
            instance_id: instance(),
        },
        &snapshot,
    );
    assert!(deletion.is_blocked());
}
