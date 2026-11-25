use super::*;

#[test]
fn generated_names_preserve_existing_identity_and_round_trip() {
    let application = EnvironmentId::parse("a".repeat(64)).expect("the environment id is valid");
    let logical = "b".repeat(63);
    let service = DockerServiceName::for_service(
        &application,
        &ServiceName::parse(&logical).expect("the service name is valid"),
    );
    let volume = DockerVolumeName::for_volume(
        &application,
        &VolumeName::parse(&logical).expect("the volume name is valid"),
    );
    let network = DockerNetworkName::for_application(&application);
    assert_eq!(
        service.as_str(),
        docker_resource_name(&application, ResourceKind::Service, Some(&logical))
    );
    assert_ne!(service.as_str(), volume.as_str());
    assert_ne!(volume.as_str(), network.as_str());
    assert_eq!(
        DockerServiceName::parse(service.as_str()).expect("the docker service name is valid"),
        service
    );
    assert_eq!(
        DockerVolumeName::parse(volume.as_str()).expect("the docker volume name is valid"),
        volume
    );
    assert_eq!(
        DockerNetworkName::parse(network.as_str()).expect("the docker network name is valid"),
        network
    );
    assert_eq!(
        serde_json::from_str::<DockerServiceName>(
            &serde_json::to_string(&service).expect("the value serializes")
        )
        .expect("the docker service name deserializes"),
        service
    );
}

/// Pins every constructor's name for one environment. Resources on
/// existing hosts carry these names, so a change here orphans them: the
/// daemon creates new ones and no longer recognizes the old. Change a
/// name only together with a migration of the resources that have it.
#[test]
fn names_stay_the_same_across_releases() {
    let id = EnvironmentId::parse("env-0000000001").expect("the environment ID is valid");
    let exporter = EnvironmentId::parse("env-0000000002").expect("the exporter ID is valid");
    let names = [
        DockerServiceName::for_service(
            &id,
            &ServiceName::parse("web").expect("the service name is valid"),
        )
        .to_string(),
        DockerServiceName::for_job(
            &id,
            &JobName::parse("migrate").expect("the job name is valid"),
        )
        .to_string(),
        DockerVolumeName::for_volume(
            &id,
            &VolumeName::parse("data").expect("the volume name is valid"),
        )
        .to_string(),
        DockerNetworkName::for_application(&id).to_string(),
        DockerNetworkName::for_ingress(&id).to_string(),
        DockerNetworkName::for_link(&id, &exporter).to_string(),
        DockerConfigName::for_file(&id, "hello\n", FileMode::READ_ONLY).to_string(),
    ];
    assert_eq!(
        names,
        [
            "piqueld-env-0000000001-service-web-76993d01aba5",
            "piqueld-env-0000000001-job-migrate-af1cf5d450f1",
            "piqueld-env-0000000001-volume-data-a7f4431b5987",
            "piqueld-env-0000000001-network-b612a964f4bd",
            "piqueld-env-0000000001-network-ingress-b2bafc607de6",
            "piqueld-env-0000000001-network-link-env-0000000002-cc3a31c438dc",
            "piqueld-env-0000000001-file-5891b5b522d5df086d0ff0-85dbd53dee1f",
        ]
    );
}
