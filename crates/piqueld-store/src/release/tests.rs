use super::*;
use crate::Actor::Daemon;
use crate::testing::prebuilt;
use crate::{Mutation, MutationResponse};
use piqueld_core::{
    EnvironmentName, InstanceId, NormalizedApplication, ResolutionSet, ResolvedSource, ServiceName,
    compile_application, manifest::ApplicationTemplate,
};
use std::collections::BTreeMap;

/// The image `web` of the [`prebuilt`] application pulls.
const IMAGE: &str = "ghcr.io/example/notes:1.4.0";

/// `IMAGE` resolved to a digest of 64 `byte`s.
fn pulled(byte: char) -> ResolvedSource {
    ResolvedSource::parse_image(
        IMAGE,
        format!(
            "ghcr.io/example/notes@sha256:{}",
            byte.to_string().repeat(64)
        ),
    )
    .expect("parse image succeeds")
}

/// `application` compiled for `environment`, with `web` pulled as `web`.
fn target(
    application: &NormalizedApplication,
    environment: &EnvironmentId,
    instance: &str,
    web: ResolvedSource,
) -> ResolvedApplication {
    compile_application(
        application,
        environment,
        InstanceId::parse(instance).expect("the instance id parses"),
        &ResolutionSet {
            sources: BTreeMap::from([(
                ServiceName::parse("web").expect("the service name parses"),
                web,
            )]),
            secret_names: BTreeMap::new(),
        },
    )
    .expect("compile application succeeds")
}

/// Saves `notes` with a `staging` environment next to `production`.
async fn save_with_staging(store: &Store) -> (ApplicationId, EnvironmentId, EnvironmentId) {
    let template = ApplicationTemplate::from(&prebuilt("app-notes-test"));
    let template = template
        .to_manifest()
        .validate_template()
        .expect("validate template succeeds");
    let saved = store.save_template(template, false).await;
    let application =
        ApplicationId::parse(saved.application_id.as_str()).expect("the application id parses");
    let create = Mutation::CreateEnvironment {
        application: application.clone(),
        name: EnvironmentName::parse("staging").expect("the environment name parses"),
        source: None,
    };
    let (MutationResponse::Environment(staging), _) = store
        .accept(Daemon, create, None, true, None)
        .await
        .expect("accept succeeds")
    else {
        panic!("environment")
    };
    let production = store.production(&application).await;
    (application, production, staging.id)
}

/// Deploys `environment` and prepares it with `web` pulled as `web`,
/// returning the deployment's ID.
async fn prepare(store: &Store, environment: &EnvironmentId, web: ResolvedSource) -> String {
    let operation = store
        .request_deploy(environment, None)
        .await
        .expect("request deploy succeeds");
    store.mark_running(&operation.id).await;
    let rendering = store
        .deployment_snapshot(&operation.id)
        .await
        .expect("deployment snapshot succeeds")
        .rendering
        .expect("rendering is present");
    let target = target(
        &rendering.application,
        environment,
        store.instance_id().as_str(),
        web,
    );
    store
        .save_prepared(&operation, &target)
        .await
        .expect("save prepared succeeds");
    operation.id
}

/// The release each of `environment`'s deployments references, by deployment ID.
async fn deployment_releases(
    store: &Store,
    environment: &EnvironmentId,
) -> BTreeMap<String, Option<ReleaseId>> {
    store
        .deployments(environment, None, 10)
        .await
        .expect("deployments succeeds")
        .items
        .into_iter()
        .map(|deployment| (deployment.operation.id, deployment.release))
        .collect()
}

#[tokio::test]
async fn identical_preparations_share_a_release_that_outlives_its_environment() {
    let (_temp, store) = Store::temporary().await;
    let (application, production, staging) = save_with_staging(&store).await;
    let first = prepare(&store, &production, pulled('a')).await;
    let shared = prepare(&store, &staging, pulled('a')).await;
    let updated = prepare(&store, &production, pulled('b')).await;

    let releases = store
        .releases(&application, None, 10)
        .await
        .expect("releases succeeds")
        .items;
    let [newer, older] = releases.as_slice() else {
        panic!("two releases, got {releases:?}")
    };
    let web = ServiceName::parse("web").expect("the service name parses");
    assert_eq!(older.release.sources()[&web], pulled('a'));
    assert_eq!(newer.release.sources()[&web], pulled('b'));
    assert_eq!(older.release.commit(), None);
    assert_eq!(
        older.fingerprint.services()[&web],
        piqueld_core::BuildInputs::new(&pulled('a').requested(), None)
    );
    assert_eq!(older.content_hash, older.release.content_hash());
    let production_releases = deployment_releases(&store, &production).await;
    assert_eq!(production_releases[&first].as_ref(), Some(&older.id));
    assert_eq!(production_releases[&updated].as_ref(), Some(&newer.id));
    assert_eq!(
        deployment_releases(&store, &staging).await[&shared].as_ref(),
        Some(&older.id)
    );

    let deletion = store
        .request_delete(&staging)
        .await
        .expect("request delete succeeds");
    store.mark_running(&deletion.id).await;
    store
        .finish_delete_operation(&deletion)
        .await
        .expect("finish delete operation succeeds");
    assert!(matches!(
        store.get(&staging).await,
        Err(StoreError::NotFound)
    ));
    let remaining = store
        .releases(&application, None, 10)
        .await
        .expect("releases succeeds")
        .items;
    assert_eq!(
        remaining
            .iter()
            .map(|release| &release.id)
            .collect::<Vec<_>>(),
        [&newer.id, &older.id]
    );
}

/// A release recorded while the clock is behind the application's newest one,
/// as after a restart with the clock behind, still comes after it.
#[tokio::test]
async fn a_release_recorded_with_the_clock_behind_comes_after_the_newest() {
    let (_temp, store) = Store::temporary().await;
    let (_, production, _) = save_with_staging(&store).await;
    prepare(&store, &production, pulled('a')).await;
    let ahead: i64 = 4_102_444_800_000;
    sqlx::query("UPDATE releases SET created_at_ms=?1")
        .bind(ahead)
        .execute(&store.pool)
        .await
        .unwrap();

    prepare(&store, &production, pulled('b')).await;
    let created: Vec<i64> =
        sqlx::query_scalar("SELECT created_at_ms FROM releases ORDER BY created_at_ms")
            .fetch_all(&store.pool)
            .await
            .unwrap();
    assert_eq!(created, [ahead, ahead + 1]);
}
