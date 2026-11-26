use super::*;
use crate::Actor::Daemon;
use crate::testing::template;
use crate::{Mutation, MutationResponse, PreviewMutation};
use crate::{Operation, OperationState};
use piqueld_core::{
    ApplicationId, EnvironmentId, GitBranch, ResolutionSet, ResolvedSource, ServiceName,
    compile_application,
};
use std::collections::BTreeMap;

/// The digest of `web`'s image in deployments marked `byte`.
fn image(byte: char) -> ImmutableImage {
    ImmutableImage::parse(format!(
        "ghcr.io/example/notes@sha256:{}",
        byte.to_string().repeat(64)
    ))
    .expect("the immutable image parses")
}

/// Saves the repository-backed `notes` application, so it can have
/// previews, and returns its production environment and a preview.
async fn production_and_preview(store: &Store) -> (EnvironmentId, EnvironmentId) {
    let template = template(
        "notes",
        "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n\
         [[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1'",
    );
    let saved = store.save_template(template, false).await;
    let application =
        ApplicationId::parse(saved.application_id).expect("the application id parses");
    let create = Mutation::Preview(PreviewMutation::Create {
        application: application.clone(),
        branch: GitBranch::parse("feat/login").expect("the git branch parses"),
        slot: None,
    });
    let (MutationResponse::Preview(preview), _) = store
        .accept(Daemon, create, None, false, None)
        .await
        .expect("accept succeeds")
    else {
        panic!("preview")
    };
    (store.production(&application).await, preview.preview.id)
}

/// Deploys `environment` and saves its prepared target, running `web` from
/// [`image`]`(byte)`.
async fn prepare(store: &Store, environment: &EnvironmentId, byte: char) -> Operation {
    let operation = store
        .request_deploy(environment, None)
        .await
        .expect("request deploy succeeds");
    store.mark_running(&operation.id).await;
    let application = crate::testing::prebuilt("app-notes-test");
    let web = ResolvedSource::parse_image("ghcr.io/example/notes:1.4.0", image(byte).as_str())
        .expect("parse image succeeds");
    let target = compile_application(
        &application,
        environment,
        store.instance_id().clone(),
        &ResolutionSet {
            sources: BTreeMap::from([(
                ServiceName::parse("web").expect("the service name parses"),
                web,
            )]),
            secret_names: BTreeMap::new(),
        },
    )
    .expect("compile application succeeds");
    store
        .save_prepared(&operation, &target)
        .await
        .expect("save prepared succeeds");
    operation
}

/// Deploys `environment` with [`image`]`(byte)` until it succeeds.
async fn deploy(store: &Store, environment: &EnvironmentId, byte: char) {
    let operation = prepare(store, environment, byte).await;
    store
        .publish_prepared(&operation)
        .await
        .expect("publish prepared succeeds");
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .expect("transition operation succeeds");
}

/// The images `root` keeps.
async fn kept(store: &Store, root: RetentionRoot, keep: u32) -> BTreeSet<ImmutableImage> {
    let mut connection = store.pool.acquire().await.expect("acquire succeeds");
    Store::retained_on(&mut connection, root, keep)
        .await
        .expect("retained on succeeds")
        .iter()
        .flat_map(ResolvedApplication::images)
        .collect()
}

#[tokio::test]
async fn each_root_keeps_its_deployments_and_previews_keep_no_history() {
    let (_temp, store) = Store::temporary().await;
    let (production, preview) = production_and_preview(&store).await;
    for byte in ['a', 'b', 'c'] {
        deploy(&store, &production, byte).await;
    }
    prepare(&store, &production, 'd').await;
    deploy(&store, &preview, 'e').await;
    prepare(&store, &preview, 'f').await;

    let images = |bytes: &str| bytes.chars().map(image).collect::<BTreeSet<_>>();
    assert_eq!(kept(&store, RetentionRoot::Current, 2).await, images("ce"));
    assert_eq!(kept(&store, RetentionRoot::Latest, 2).await, images("df"));
    assert_eq!(kept(&store, RetentionRoot::Recent, 2).await, images("bc"));
    assert_eq!(kept(&store, RetentionRoot::Recent, 0).await, images(""));
    assert_eq!(
        kept(&store, RetentionRoot::PromotionSource, 2).await,
        images("")
    );
    let create = Mutation::CreateEnvironment {
        application: store
            .get(&production)
            .await
            .expect("the environment exists")
            .environment
            .application_id,
        name: piqueld_core::EnvironmentName::parse("canary").expect("the environment name parses"),
        source: Some(crate::SourceChoice::PromoteFrom(production.clone())),
    };
    store.force(create).await.expect("force succeeds");
    assert_eq!(
        kept(&store, RetentionRoot::PromotionSource, 2).await,
        images("c")
    );
    assert_eq!(
        store
            .retained_images(2)
            .await
            .expect("retained images succeeds"),
        images("bcdef")
    );
    assert_eq!(
        store
            .retained_images(3)
            .await
            .expect("retained images succeeds"),
        images("abcdef")
    );
}
