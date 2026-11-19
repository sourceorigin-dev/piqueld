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
    let id = EnvironmentId::parse("app-notes-01").unwrap();
    let instance = instance();
    let network = DesiredNetwork {
        name: piqueld_core::DockerNetworkName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Network,
            None,
        ))
        .unwrap(),
        labels: labels_for(&instance, &id),
    };
    assert!(network.has_valid_identity());

    // A service label on a network breaks the role separation.
    let mut mislabeled = network.clone();
    mislabeled
        .labels
        .insert("io.piqueld.service".into(), "web".into());
    assert!(!mislabeled.has_valid_identity());

    // An off-canonical name is rejected.
    let mut renamed = network.clone();
    renamed.name = piqueld_core::DockerNetworkName::parse(format!("{}x", renamed.name)).unwrap();
    assert!(!renamed.has_valid_identity());

    let volume = DesiredVolume {
        logical_name: piqueld_core::VolumeName::parse("data").unwrap(),
        name: piqueld_core::DockerVolumeName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Volume,
            Some("data"),
        ))
        .unwrap(),
        labels: labels_for(&instance, &id),
    };
    assert!(volume.has_valid_identity());

    let resolved = resolutions().sources.get("web").unwrap().clone();
    let service = DesiredService {
        link_aliases: BTreeMap::new(),
        secrets: Vec::new(),
        files: Vec::new(),
        logical_name: piqueld_core::ServiceName::parse("web").unwrap(),
        name: piqueld_core::DockerServiceName::parse(piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Service,
            Some("web"),
        ))
        .unwrap(),
        source: resolved,
        image: piqueld_core::ImmutableImage::parse(format!("ghcr.io/example/notes@{}", digest()))
            .unwrap(),
        replicas: 1,
        environment: BTreeMap::default(),
        command: Vec::new(),
        arguments: Vec::new(),
        mounts: Vec::new(),
        healthcheck: None,
        resources: None,
        depends_on: Vec::new(),
        rollout: Rollout::default(),
        receives_telemetry: false,
        telemetry: None,
        networks: vec![
            piqueld_core::DockerNetworkName::parse(piqueld_core::docker_resource_name(
                &id,
                piqueld_core::ResourceKind::Network,
                None,
            ))
            .unwrap(),
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
    let id = EnvironmentId::parse("app-notes-01").unwrap();
    let instance = instance();

    assert_eq!(
        OwnershipState::from_labels(&labels_for(&instance, &id), &instance, &id),
        OwnershipState::Owned
    );

    let foreign = OwnershipState::from_labels(
        &labels_for(&InstanceId::parse("other-instance").unwrap(), &id),
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
    let mut desired =
        compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
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

    // Services deployed before aliases existed register as drift and gain them.
    let mut unaliased = observed(&desired);
    for attachment in &mut unaliased.services[0].networks {
        attachment.aliases.clear();
    }
    assert!(!unaliased.services[0].matches(&desired.services[0]));

    // The shared gateway joins the ingress network, so an alias there is drift.
    let mut exposed = observed(&desired);
    exposed.services[0].networks[1].aliases = vec!["web".into()];
    assert!(!exposed.services[0].matches(&desired.services[0]));
}

#[test]
fn compiled_ownership_carries_the_spec_hash_label() {
    let app = application();
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let hash = Sha256Digest::parse(desired.spec_hash.clone()).unwrap();
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
    let desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
    let mut wire = serde_json::to_value(&desired).unwrap();
    wire["services"][0]["name"] = "Foreign_Service".into();
    assert!(serde_json::from_value::<piqueld_core::ResolvedApplication>(wire).is_err());

    let mut snapshot = serde_json::to_value(observed(&desired)).unwrap();
    snapshot["services"][0]["name"] = "Foreign_Service".into();
    snapshot["services"][0]["mounts"] = serde_json::json!([
        {"volume_name":"Foreign.Volume", "target":"/data", "read_only":false}
    ]);
    let observation: ObservedApplication = serde_json::from_value(snapshot).unwrap();
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
    let desired =
        compile_application(&application(), &environment(), instance(), &resolutions()).unwrap();
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
                unreachable!("observation never reports jobs or configs")
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
