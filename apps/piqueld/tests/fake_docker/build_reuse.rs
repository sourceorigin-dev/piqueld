use super::*;
use piqueld_core::api::{BuildState, BuiltOn};

impl ControllerHarness {
    /// Deploys `environment` again, building every Git source when
    /// `rebuild` is set, and waits for it to succeed.
    async fn redeploy(&self, environment: &EnvironmentId, rebuild: bool) {
        self.finish(&self.request_deploy(environment, None, rebuild).await)
            .await;
    }

    /// How many images the engine built.
    async fn image_builds(&self) -> u64 {
        self.docker.registry.lock().await.pulls["git-build"]
    }
}

#[tokio::test]
async fn a_deployment_reuses_the_image_of_an_unchanged_build_unless_it_rebuilds_or_the_image_is_gone()
 {
    let harness = ControllerHarness::new().await;
    let fixture = git_fixture::GitBuildFixture::new();
    let first = harness.deploy_git_build(&fixture).await;
    harness.finish(&first).await;
    let environment = &first.environment_id;
    assert_eq!(harness.image_builds().await, 1);

    // The same commit and build settings: nothing is built.
    harness.redeploy(environment, false).await;
    assert_eq!(harness.image_builds().await, 1);
    let builds = harness.builds(environment).await;
    let (reused, built) = (&builds[0], &builds[1]);
    assert_eq!(reused.state, BuildState::Succeeded);
    assert_eq!(reused.reused_build, Some(built.id));
    assert_eq!(
        (built.reused_build, &reused.image_id),
        (None, &built.image_id)
    );
    // Only a build that built an image ran somewhere.
    assert_eq!(
        (built.built_on, reused.built_on),
        (Some(BuiltOn::Host), None)
    );
    let output = harness
        .store
        .build_logs(reused.id, None, None)
        .await
        .unwrap();
    assert!(
        output
            .items
            .iter()
            .any(|chunk| chunk.text.contains(&format!("build #{}", built.id)))
    );

    harness.redeploy(environment, true).await;
    assert_eq!(harness.image_builds().await, 2);
    assert_eq!(harness.builds(environment).await[0].reused_build, None);

    // An image cleanup removed, or someone pruned, has to be built again.
    harness.docker.local_images.lock().await.clear();
    harness.redeploy(environment, false).await;
    assert_eq!(harness.image_builds().await, 3);
    assert_eq!(harness.builds(environment).await[0].reused_build, None);
}

#[tokio::test]
async fn a_build_machine_image_runs_by_its_repository_digest_and_is_reused() {
    let repository = piqueld_core::build_machine::BuildRepository::try_from(
        "build.example.ts.net:5000/piqueld".to_owned(),
    )
    .unwrap();
    let harness = ControllerHarness::with_docker(FakeDocker {
        build_repository: Some(repository),
        ..FakeDocker::default()
    })
    .await;
    let fixture = git_fixture::GitBuildFixture::new();
    let first = harness.deploy_git_build(&fixture).await;
    harness.finish(&first).await;
    let environment = &first.environment_id;
    let pushed = format!(
        "build.example.ts.net:5000/piqueld@sha256:{}",
        "b".repeat(64)
    );
    let built = harness.builds(environment).await.remove(0);
    assert_eq!(built.image_id.as_deref(), Some(pushed.as_str()));
    assert_eq!(built.built_on, Some(BuiltOn::BuildMachine));
    let running = || async {
        harness
            .store
            .get(environment)
            .await
            .unwrap()
            .resolved
            .unwrap()
            .services[0]
            .image
            .to_string()
    };
    assert_eq!(running().await, pushed);

    // The engine has it under its repository digest, so it is reused.
    harness.redeploy(environment, false).await;
    assert_eq!(harness.image_builds().await, 1);
    assert_eq!(
        harness.builds(environment).await[0].reused_build,
        Some(built.id)
    );
    assert_eq!(running().await, pushed);
}
