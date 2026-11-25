use super::*;
use piqueld::reconcile::ImagesInUse;
use piqueld_core::{
    RepositoryDigest, ServiceName,
    api::{ImageStatus, RegistryImageStatus},
    build_machine::BuildRepository,
};
use piqueld_docker::BuildRegistry;
use std::collections::BTreeSet;

/// A local image with ID `byte`s and `labels`.
fn image(byte: char, labels: &[(&str, &str)]) -> LocalImage {
    LocalImage {
        id: Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64)))
            .expect("the sha256 digest parses"),
        repo_digests: Vec::new(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        size: 10,
        used: false,
    }
}

impl ControllerHarness {
    /// IDs of the images left in the engine.
    async fn image_ids(&self) -> Vec<char> {
        self.docker
            .local_images
            .lock()
            .await
            .iter()
            .map(|image| {
                image
                    .id
                    .as_str()
                    .chars()
                    .last()
                    .expect("the last item exists")
            })
            .collect()
    }

    /// The resources of the daemon actions in `phase` that ended as `kind`,
    /// such as `action_succeeded`.
    async fn actions(&self, phase: piqueld_core::Phase, kind: &str) -> Vec<String> {
        let events = self
            .store
            .events(None, None, 100)
            .await
            .expect("the events list")
            .items;
        events
            .into_iter()
            .filter(|event| event.phase == Some(phase) && event.kind == kind)
            .filter_map(|event| event.resource)
            .collect()
    }

    /// A harness whose builds a build machine pushes to `repository`, with
    /// `web` built, pushed and running, as `b`s.
    async fn with_build_machine(repository: &BuildRepository) -> Self {
        let harness = Self::with_docker(FakeDocker {
            build_repository: Some(repository.clone()),
            ..FakeDocker::default()
        })
        .await;
        let fixture = git_fixture::GitBuildFixture::new();
        let operation = harness.deploy_git_build(&fixture).await;
        harness.finish(&operation).await;
        harness
    }

    /// Pushes `images` to the build registry under this installation's
    /// tags, as its builds would.
    async fn push(&self, images: &[&RepositoryDigest]) {
        let instance = self.store.instance_id();
        let mut registry = self.docker.pushed_images.lock().await;
        for image in images {
            registry.insert(BuildRegistry::new_tag(instance), (*image).clone());
        }
    }

    /// The images the build registry holds, under any tag.
    async fn registry_images(&self) -> BTreeSet<RepositoryDigest> {
        let registry = self.docker.pushed_images.lock().await;
        registry.values().cloned().collect()
    }

    /// The registry's status as of the last cleanup.
    fn registry_status(&self) -> RegistryImageStatus {
        let status = self.controller.image_status().borrow().clone();
        status.registry.expect("the registry is present")
    }
}

#[tokio::test]
async fn cleanup_removes_only_this_installations_unretained_builds() {
    let harness = ControllerHarness::new().await;
    let fixture = git_fixture::GitBuildFixture::new();
    let operation = harness.deploy_git_build(&fixture).await;
    harness.finish(&operation).await;
    let ours = harness.store.instance_id().to_owned();
    let owned = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, ours.as_str())];
    let used = LocalImage {
        used: true,
        ..image('f', &owned)
    };
    harness.docker.local_images.lock().await.extend([
        image('c', &owned),
        image(
            'd',
            &[(MANAGED_LABEL, "true"), (INSTANCE_LABEL, "instance-other")],
        ),
        image('e', &[]),
        used,
    ]);

    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(harness.image_ids().await, ['b', 'd', 'e', 'f']);
    let status = harness.controller.image_status().borrow().clone();
    assert_eq!((status.images, status.reclaimed_bytes), (2, 10));
    assert_eq!(
        harness
            .actions(piqueld_core::Phase::RemoveImage, "action_succeeded")
            .await,
        [image('c', &[]).id.as_str()]
    );
}

#[tokio::test]
async fn an_image_built_for_a_deployment_in_preparation_survives_cleanup() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        build_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let fixture = git_fixture::GitBuildFixture::new();
    let operation = harness.deploy_git_build(&fixture).await;
    let cancellation = CancellationToken::new();
    let scan = harness.controller.scan(&cancellation);
    tokio::pin!(scan);
    tokio::select! {
        () = gate.entered.notified() => {}
        result = &mut scan => panic!("scan finished before building: {result:?}"),
    }
    assert_eq!(harness.image_ids().await, ['b']);
    assert!(
        !harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(harness.image_ids().await, ['b']);
    assert_eq!(
        harness.controller.image_status().borrow().clone(),
        ImageStatus::default()
    );

    gate.release.notify_one();
    scan.await.expect("the scan is present");
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(harness.image_ids().await, ['b']);
}

#[tokio::test]
async fn missing_images_are_pulled_again_by_digest_or_reported_unavailable() {
    let harness = ControllerHarness::new().await;
    let in_use: ImagesInUse = harness.controller.images_in_use().await;
    let web = ServiceName::parse("web").expect("the service name parses");
    let pulled = prebuilt::resolutions(&harness.application).sources[&web].clone();
    let digest = pulled
        .repository_digest()
        .expect("repository digest succeeds")
        .to_string();

    in_use
        .ensure(&*harness.docker, [(&web, &pulled)])
        .await
        .expect("ensure succeeds");
    assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);

    let api = ServiceName::parse("api").expect("the service name parses");
    let built = ResolvedSource::Git {
        requested: piqueld_core::manifest::ValidatedSource::Image {
            image: "unused".into(),
        },
        commit: "a".repeat(40),
        image_id: image('c', &[]).id.into(),
    };
    let error = in_use
        .ensure(&*harness.docker, [(&web, &pulled), (&api, &built)])
        .await
        .expect_err("ensure is rejected");
    assert_eq!(error.code(), "image_unavailable");
    assert_eq!(
        error.message(),
        format!(
            "service api's image sha256:{} is no longer present, and a build can't be pulled again",
            "c".repeat(64)
        )
    );
    assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);
}

#[tokio::test]
async fn cleanup_deletes_unretained_pushed_images_from_a_reachable_registry() {
    let repository = build_repository();
    let harness = ControllerHarness::with_build_machine(&repository).await;
    let [running, unused, on_host, others] = pushed(&repository, ['b', 'c', 'd', 'e']);
    harness.push(&[&unused, &on_host]).await;
    let docker = &harness.docker;
    let other_tag = "instance-other.0".to_owned();
    docker
        .pushed_images
        .lock()
        .await
        .insert(other_tag, others.clone());
    let ours = harness.store.instance_id().to_owned();
    let owned = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, ours.as_str())];
    docker.local_images.lock().await.push(LocalImage {
        repo_digests: vec![on_host.clone()],
        used: true,
        ..image('d', &owned)
    });
    docker.registry_unreachable.store(true, Ordering::SeqCst);
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    let all = [
        running.clone(),
        unused.clone(),
        on_host.clone(),
        others.clone(),
    ];
    assert_eq!(harness.registry_images().await, all.into());
    let status = harness.registry_status();
    assert_eq!((status.images, status.deleted_images), (None, 0));
    assert_eq!(
        status.failure.as_deref(),
        Some("the registry at registry.example.ts.net:5000 is unreachable: connection refused")
    );

    docker.registry_unreachable.store(false, Ordering::SeqCst);
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    let kept = [running, on_host, others];
    assert_eq!(harness.registry_images().await, kept.into());
    assert_eq!(
        harness.registry_status(),
        RegistryImageStatus {
            repository: repository.to_string(),
            images: Some(2),
            deleted_images: 1,
            failure: None,
        }
    );
    assert_eq!(
        harness
            .actions(piqueld_core::Phase::DeleteRegistryImage, "action_succeeded")
            .await,
        [unused.as_str()]
    );
}

#[tokio::test]
async fn registry_cleanup_goes_past_one_images_failure_but_stops_at_the_registrys() {
    let repository = build_repository();
    let harness = ControllerHarness::with_build_machine(&repository).await;
    let [running, c, d, e, f] = pushed(&repository, ['b', 'c', 'd', 'e', 'f']);
    harness.push(&[&c, &e, &f]).await;
    let docker = &harness.docker;
    let refuse = |image: &RepositoryDigest, status| {
        let image = image.clone();
        async move {
            docker.refused_deletes.lock().await.insert(image, status);
        }
    };

    refuse(&c, 500).await;
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(
        harness.registry_images().await,
        [running.clone(), c.clone()].into()
    );
    let status = harness.registry_status();
    assert_eq!((status.images, status.deleted_images), (Some(2), 2));
    assert!(
        status
            .failure
            .expect("the failure is present")
            .contains("500 Internal Server Error")
    );

    harness.push(&[&d]).await;
    refuse(&c, 405).await;
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(
        harness.registry_images().await,
        [running.clone(), c.clone(), d.clone()].into()
    );
    let status = harness.registry_status();
    assert_eq!((status.images, status.deleted_images), (Some(3), 2));
    assert!(
        status
            .failure
            .expect("the failure is present")
            .contains("405 Method Not Allowed")
    );
    let failed = harness
        .actions(piqueld_core::Phase::DeleteRegistryImage, "action_failed")
        .await;
    assert_eq!(failed, [c.as_str(), c.as_str()]);

    harness.push(&[&e, &f]).await;
    for image in [&c, &d, &e] {
        refuse(image, 500).await;
    }
    assert!(
        harness
            .controller
            .clean_images()
            .await
            .expect("clean images succeeds")
    );
    assert_eq!(
        harness.registry_images().await,
        [running, c, d, e, f].into()
    );
    assert_eq!(harness.registry_status().images, Some(5));
}

/// The repository the harness's build machine pushes to.
fn build_repository() -> BuildRepository {
    BuildRepository::try_from("registry.example.ts.net:5000/piqueld".to_owned())
        .expect("the build repository converts")
}

/// The images `repository` holds under digests of `bytes`.
fn pushed<const N: usize>(repository: &BuildRepository, bytes: [char; N]) -> [RepositoryDigest; N] {
    bytes.map(|byte| {
        repository.image(
            &Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64)))
                .expect("the sha256 digest parses"),
        )
    })
}
