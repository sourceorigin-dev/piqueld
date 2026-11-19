//! Link networks between importing and exporting environments.

use std::collections::BTreeMap;

use piqueld_core::planner::{ActionKind, PlanRequest};
use piqueld_core::resource::{
    NetworkAttachment, ObservedApplication, ObservedNetwork, ResolvedApplication,
    compile_application,
};
use piqueld_core::{DockerNetworkName, EnvironmentId, Plan};

use super::{application, instance, observed, resolutions};

/// An importer and an exporter environment, and the link between them.
fn linked_environments() -> (
    ResolvedApplication,
    ResolvedApplication,
    piqueld_core::links::Link,
) {
    let compiled = |id: &str| {
        compile_application(
            &application(),
            &EnvironmentId::parse(id).unwrap(),
            instance(),
            &resolutions(),
        )
        .unwrap()
    };
    let importer = compiled("env-importer");
    let exporter = compiled("env-exporter");
    let link = piqueld_core::links::Link {
        importer: importer.id.clone(),
        exporter: exporter.id.clone(),
        exporter_application: piqueld_core::ApplicationName::parse("auth").unwrap(),
        services: [piqueld_core::ServiceName::parse("web").unwrap()].into(),
    };
    (importer, exporter, link)
}

#[test]
fn links_join_the_importer_and_only_imported_services_on_the_importers_network() {
    use piqueld_core::resource::LINK_LABEL;
    let (importer, exporter, link) = linked_environments();
    let name = DockerNetworkName::for_link(&importer.id, &exporter.id);

    // The importer owns the network, which all of its services join unaliased.
    let as_importer = importer
        .clone()
        .with_links(std::slice::from_ref(&link), &ObservedApplication::default());
    let network = as_importer
        .networks
        .iter()
        .find(|network| network.name == name)
        .expect("the importer owns the link network");
    assert!(network.has_valid_identity());
    assert_eq!(network.labels[LINK_LABEL], exporter.id.as_str());
    assert!(
        as_importer.services[0]
            .network_attachments()
            .contains(&NetworkAttachment {
                network: name.to_string(),
                aliases: Vec::new(),
            })
    );

    // The exporter joins only a network its importer owns, under its alias.
    let linked = |labels: BTreeMap<String, String>| ObservedApplication {
        linked_networks: vec![ObservedNetwork {
            name: name.to_string(),
            runtime_configuration_matches: true,
            labels,
            in_use: true,
        }],
        ..ObservedApplication::default()
    };
    assert_eq!(
        exporter
            .clone()
            .with_links(std::slice::from_ref(&link), &ObservedApplication::default()),
        exporter,
        "nothing joins a link network that does not exist yet"
    );
    let mut forged = network.labels.clone();
    forged.insert(
        piqueld_core::resource::APPLICATION_LABEL.into(),
        "env-other".into(),
    );
    assert_eq!(
        exporter
            .clone()
            .with_links(std::slice::from_ref(&link), &linked(forged)),
        exporter,
        "a network another environment owns is never joined"
    );
    let as_exporter = exporter
        .clone()
        .with_links(std::slice::from_ref(&link), &linked(network.labels.clone()));
    assert!(
        as_exporter
            .networks
            .iter()
            .all(|network| network.name != name)
    );
    assert!(
        as_exporter.services[0]
            .network_attachments()
            .contains(&NetworkAttachment {
                network: name.to_string(),
                aliases: vec!["web.auth".into()],
            })
    );
}

/// Once the link is gone, the importer keeps its network while the
/// exporter's containers are attached, then removes it. Deleting the
/// importer leaves it behind instead of waiting for the exporter.
#[test]
fn importers_remove_link_networks_once_nothing_is_attached() {
    let (importer, exporter, link) = linked_environments();
    let name = DockerNetworkName::for_link(&importer.id, &exporter.id);
    let network = importer
        .clone()
        .with_links(std::slice::from_ref(&link), &ObservedApplication::default())
        .networks
        .into_iter()
        .find(|network| network.name == name)
        .unwrap();
    let mut runtime = observed(&importer);
    runtime.networks.push(ObservedNetwork {
        name: name.to_string(),
        runtime_configuration_matches: true,
        labels: network.labels.clone(),
        in_use: true,
    });
    let request = PlanRequest::Reconcile {
        desired: importer.clone(),
    };
    let removes = |observed: &ObservedApplication| {
        Plan::from_request(&request, observed)
            .actions
            .iter()
            .any(|action| {
                matches!(&action.kind, ActionKind::RemoveNetwork { name: removed } if *removed == name.as_str())
            })
    };
    assert!(!removes(&runtime));
    let deletion = Plan::from_request(
        &PlanRequest::Delete {
            environment_id: importer.id.clone(),
            instance_id: instance(),
        },
        &runtime,
    );
    assert!(!deletion.is_blocked());
    let deleted_networks = deletion
        .actions
        .iter()
        .filter_map(|action| match &action.kind {
            ActionKind::RemoveNetwork { name } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!deleted_networks.is_empty() && !deleted_networks.contains(&name.as_str()));
    runtime.networks.last_mut().unwrap().in_use = false;
    assert!(removes(&runtime));
}

/// Services leave link networks as soon as their links stop granting them,
/// whether they import through the network or are exported on it.
#[test]
fn services_lose_link_networks_their_links_no_longer_grant() {
    let (importer, exporter, link) = linked_environments();
    let as_importer = importer
        .clone()
        .with_links(std::slice::from_ref(&link), &ObservedApplication::default());
    let mut runtime = observed(&as_importer);
    let network = runtime
        .networks
        .iter()
        .find(|network| {
            network
                .labels
                .contains_key(piqueld_core::resource::LINK_LABEL)
        })
        .cloned()
        .unwrap();
    let web = as_importer.services[0].name.as_str();
    assert_eq!(
        runtime.revoked_links(
            &instance(),
            &importer.id,
            std::slice::from_ref(&as_importer)
        ),
        Vec::new()
    );
    assert_eq!(
        runtime.revoked_links(&instance(), &importer.id, std::slice::from_ref(&importer)),
        [(web, network.name.as_str())]
    );
    assert_eq!(
        runtime.revoked_links(&instance(), &importer.id, &[importer.clone(), as_importer]),
        Vec::new(),
        "a link the prepared target is granted stays open before its promotion"
    );

    runtime.linked_networks = vec![network.clone()];
    let as_exporter = exporter
        .clone()
        .with_links(std::slice::from_ref(&link), &runtime);
    let mut runtime = observed(&as_exporter);
    runtime.linked_networks = vec![network.clone()];
    let web = as_exporter.services[0].name.as_str();
    assert_eq!(
        runtime.revoked_links(
            &instance(),
            &exporter.id,
            std::slice::from_ref(&as_exporter)
        ),
        Vec::new()
    );
    for granted in [std::slice::from_ref(&exporter), &[]] {
        assert_eq!(
            runtime.revoked_links(&instance(), &exporter.id, granted),
            [(web, network.name.as_str())]
        );
    }
    assert_eq!(
        runtime.revoked_links(&instance(), &importer.id, &[]),
        Vec::new(),
        "only the environment's own services are detached"
    );
}
