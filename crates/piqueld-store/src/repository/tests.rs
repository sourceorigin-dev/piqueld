use super::*;
use crate::Actor::Daemon;
use crate::EnvironmentError;
use crate::testing::template;
use crate::{Mutation, MutationResponse};
use piqueld_core::{ApplicationId, EnvironmentId, OperationState, ResolutionSet};

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The scenario reads as one story, and its expect messages wrapped it past the limit."
)]
async fn fetched_snapshot_preserves_intervening_saved_configuration() {
    let (_directory, store) = Store::temporary().await;
    let initial = template(
        "test",
        "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/app.git'\nbranch='main'",
    );
    let saved = store.save_template(initial, true).await;
    let op = store
        .operation(&saved.operation_id.expect("operation id is present"))
        .await
        .expect("operation succeeds");
    store.mark_running(&op.id).await;
    let captured = store
        .deployment_snapshot(&op.id)
        .await
        .expect("deployment snapshot succeeds")
        .template;
    let mut fetched = captured.to_manifest();
    fetched.spec.manifest = None;
    let fetched = fetched
        .validate_template()
        .expect("validate template succeeds")
        .normalize(captured.id().clone());
    let environment = store
        .get(&op.environment_id)
        .await
        .expect("the environment exists");
    let rendering = environment
        .render(&fetched, &op.id)
        .expect("render succeeds");
    store
        .save_deployment_input(
            &op,
            (&fetched, Some(&fetched)),
            &rendering,
            (Some(&"a".repeat(40)), None),
            &[],
        )
        .await
        .expect("save deployment input succeeds");
    let mut edited = captured.to_manifest();
    edited.spec.manifest.as_mut().expect("as mut succeeds").path = "fixed.toml".into();
    let edited = edited
        .validate_template()
        .expect("validate template succeeds")
        .normalize(captured.id().clone());
    store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(edited.clone()),
                expected_application_id: Some(captured.id().to_string()),
                deploy: false,
            },
            Some(1),
            false,
            None,
        )
        .await
        .expect("accept succeeds");
    let target = piqueld_core::compile_application(
        &rendering.application,
        &op.environment_id,
        store.instance_id().clone(),
        &ResolutionSet::default(),
    )
    .expect("compile application succeeds");
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let saved = store
        .get(&op.environment_id)
        .await
        .expect("the environment exists")
        .application;
    assert_eq!((saved.application, saved.generation), (edited.clone(), 2));
    assert_eq!(
        store
            .operation(&op.id)
            .await
            .expect("operation succeeds")
            .generation,
        1
    );
    let snapshot = store
        .deployment_snapshot(&op.id)
        .await
        .expect("deployment snapshot succeeds");
    assert_eq!(snapshot.template, fetched);
    assert_eq!(snapshot.rendering, Some(rendering.clone()));
    assert_eq!(
        store
            .deployments(&op.environment_id, None, 3)
            .await
            .expect("deployments succeeds")
            .items[0]
            .application,
        Some(rendering.application.clone())
    );
    assert!(matches!(
        store
            .save_deployment_input(&op, (&edited, None), &rendering, (None, None), &[])
            .await,
        Err(StoreError::IllegalTransition)
    ));
}

/// A repository-backed application `notes` whose saved manifest is only
/// its connection to `main`.
fn connection() -> piqueld_core::manifest::ValidatedTemplate {
    template(
        "notes",
        "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'",
    )
}

/// Deploys `environment` from a fetched manifest with `spec`, as the
/// controller does, through preparation.
async fn fetch(store: &Store, environment: &EnvironmentId, spec: &str) {
    let (op, target) = fetched(store, environment, spec).await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
}

/// Starts a deployment of `environment` and fetches a manifest with
/// `spec`, returning the operation and the target it prepares.
async fn fetched(
    store: &Store,
    environment: &EnvironmentId,
    spec: &str,
) -> (Operation, piqueld_core::ResolvedApplication) {
    let (MutationResponse::Operation(accepted), _) = store
        .accept(
            Daemon,
            Mutation::deploy(environment.clone()),
            None,
            true,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("operation")
    };
    prepared(store, environment, &accepted.operation_id, spec).await
}

/// Runs operation `id` of `environment` through fetching a manifest with
/// `spec`, returning it and the target it prepares.
async fn prepared(
    store: &Store,
    environment: &EnvironmentId,
    id: &str,
    spec: &str,
) -> (Operation, piqueld_core::ResolvedApplication) {
    let (op, fetched, rendering) = rendered(store, environment, id, spec).await;
    store
        .save_deployment_input(
            &op,
            (&fetched, Some(&fetched)),
            &rendering,
            (Some(&"a".repeat(40)), None),
            &[],
        )
        .await
        .expect("save deployment input succeeds");
    let resolutions = ResolutionSet {
        sources: [(
            piqueld_core::ServiceName::parse("web").expect("the service name parses"),
            piqueld_core::ResolvedSource::parse_image(
                "ghcr.io/example/notes:1.4.0",
                format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            )
            .expect("parse image succeeds"),
        )]
        .into(),
        ..ResolutionSet::default()
    };
    let target = piqueld_core::compile_application(
        &rendering.application,
        environment,
        store.instance_id().clone(),
        &resolutions,
    )
    .expect("compile application succeeds");
    (op, target)
}

/// Marks operation `id` of `environment` running and renders a manifest
/// with `spec` fetched for it, returning the operation, the manifest and
/// its rendering.
async fn rendered(
    store: &Store,
    environment: &EnvironmentId,
    id: &str,
    spec: &str,
) -> (Operation, ApplicationTemplate, Rendering) {
    let op = store.operation(id).await.expect("operation succeeds");
    store.mark_running(&op.id).await;
    let current = store
        .get(environment)
        .await
        .expect("the environment exists");
    let mut fetched = piqueld_core::manifest::ApplicationManifest::decode_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{spec}"
    ))
    .expect("decode toml succeeds");
    fetched.spec.manifest = current.tracking().expect("tracking succeeds").repository();
    let fetched = fetched
        .validate_template()
        .expect("validate template succeeds")
        .normalize(current.application.application.id().clone());
    let rendering = current.render(&fetched, &op.id).expect("render succeeds");
    (op, fetched, rendering)
}

/// A fetch saved for superseded work stores none of the secret values
/// generated for it, so rejected candidates never use up the quota.
#[tokio::test]
async fn rejected_fetches_store_no_generated_secrets() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let deploy = async || {
        let (MutationResponse::Operation(accepted), _) = store
            .accept(
                Daemon,
                Mutation::deploy(production.clone()),
                None,
                true,
                None,
            )
            .await
            .expect("accept succeeds")
        else {
            panic!("operation")
        };
        accepted.operation_id
    };
    let first = deploy().await;
    let spec = format!(
        "{}\n[[spec.services.secrets]]\nname='token'\ntarget='/run/secrets/token'",
        token_spec(true)
    );
    let (op, fetched, rendering) = rendered(&store, &production, &first, &spec).await;
    deploy().await;
    assert!(matches!(
        store
            .save_deployment_input(&op, (&fetched, None), &rendering, (None, None), &[])
            .await,
        Err(StoreError::IllegalTransition)
    ));
    assert!(
        store
            .secrets(&production)
            .await
            .expect("secrets succeeds")
            .is_empty()
    );
}

/// A manifest fetched before a rename is accepted under the new name, as
/// when a deployment is retried.
#[tokio::test]
async fn fetched_manifests_keep_the_current_name() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (op, target) = fetched(&store, &production, &token_spec(false)).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("build_failed", "build failed")),
        )
        .await
        .expect("transition operation succeeds");
    let repository = |repository| Mutation::Edit {
        id: application.clone(),
        edit: Box::new(piqueld_core::edit::ApplicationEdit::Repository(repository)),
        deploy: false,
    };
    for mutation in [
        repository(None),
        crate::testing::rename_application(&application, "journal"),
        repository(connection().spec().manifest.clone()),
    ] {
        store.force(mutation).await.expect("force succeeds");
    }
    let op = store
        .retry_operation(&op)
        .await
        .expect("retry operation succeeds");
    store.mark_running(&op.id).await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let current = store
        .get(&production)
        .await
        .expect("the environment exists");
    assert_eq!(
        current.application.application.metadata().name.as_str(),
        "journal"
    );
    assert_eq!(
        current
            .manifest()
            .expect("manifest succeeds")
            .metadata()
            .name
            .as_str(),
        "journal"
    );
}

/// A manifest fetched before the connection changed never replaces the
/// configuration saved since: it came from the old path.
#[tokio::test]
async fn fetches_requested_before_a_save_keep_the_saved_configuration() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (op, target) = fetched(&store, &production, &token_spec(false)).await;
    store
        .force(Mutation::Edit {
            id: application.clone(),
            edit: Box::new(piqueld_core::edit::ApplicationEdit::RepositoryPath(
                "fixed.toml".into(),
            )),
            deploy: false,
        })
        .await
        .expect("force succeeds");
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let saved = store
        .get(&production)
        .await
        .expect("the environment exists")
        .application
        .application;
    assert_eq!(
        saved
            .spec()
            .manifest
            .as_ref()
            .expect("the value is present")
            .path,
        "fixed.toml"
    );
    assert_eq!(saved.spec().services, Vec::new());
}

/// Environment changes advance the revision, but leave the connection a
/// manifest was fetched from, so one fetched meanwhile is still saved.
#[tokio::test]
async fn fetches_are_saved_across_environment_changes() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (op, target) = fetched(&store, &production, &token_spec(false)).await;
    store
        .force(Mutation::CreateEnvironment {
            application,
            name: piqueld_core::EnvironmentName::parse("staging")
                .expect("the environment name parses"),
            source: None,
        })
        .await
        .expect("force succeeds");
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let saved = store
        .get(&production)
        .await
        .expect("the environment exists")
        .application;
    assert!(saved.generation > op.generation);
    assert_eq!(saved.application.spec().services.len(), 1);
}

/// A deployment that fetched the manifest the application had read when
/// it was requested saves nothing, even when it finishes after a push
/// and edits were saved since.
#[tokio::test]
async fn unchanged_fetches_keep_newer_edits() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    fetch(&store, &production, &token_spec(false)).await;
    let MutationResponse::Environment(staging) = store
        .force(Mutation::CreateEnvironment {
            application: application.clone(),
            name: piqueld_core::EnvironmentName::parse("staging")
                .expect("the environment name parses"),
            source: None,
        })
        .await
        .expect("force succeeds")
    else {
        panic!("environment")
    };
    let MutationResponse::Operation(delayed) = store
        .force(Mutation::deploy(staging.id.clone()))
        .await
        .expect("force succeeds")
    else {
        panic!("operation")
    };
    fetch(
        &store,
        &production,
        &format!("{}\n[[spec.volumes]]\nname='data'", token_spec(false)),
    )
    .await;
    store
        .force(Mutation::Edit {
            id: application.clone(),
            edit: Box::new(piqueld_core::edit::ApplicationEdit::Service {
                name: "web".into(),
                edit: piqueld_core::edit::ServiceEdit::Replicas(3.into()),
            }),
            deploy: false,
        })
        .await
        .expect("force succeeds");
    let (op, target) = prepared(
        &store,
        &staging.id,
        &delayed.operation_id,
        &token_spec(false),
    )
    .await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let saved = store
        .application(&application)
        .await
        .expect("application succeeds")
        .application;
    assert_eq!(saved.spec().services[0].replicas, 3.into());
    assert_eq!(saved.spec().volumes.len(), 1, "the push is kept");
}

/// A fetch is compared without the repository files it captured: the
/// manifest last read keeps none. Deployments fetched before the
/// migration recorded what they deployed, files included.
#[tokio::test]
async fn captured_files_are_not_a_push() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let spec = token_spec(false);
    fetch(&store, &production, &spec).await;
    store
        .force(Mutation::Edit {
            id: application.clone(),
            edit: Box::new(piqueld_core::edit::ApplicationEdit::Service {
                name: "web".into(),
                edit: piqueld_core::edit::ServiceEdit::Replicas(3.into()),
            }),
            deploy: false,
        })
        .await
        .expect("force succeeds");
    let MutationResponse::Operation(accepted) = store
        .force(Mutation::deploy(production.clone()))
        .await
        .expect("force succeeds")
    else {
        panic!("operation")
    };
    let (op, target) = prepared(&store, &production, &accepted.operation_id, &spec).await;
    let id = op.id.as_str();
    let json = sqlx::query_scalar!(
        "SELECT application_json FROM deployment_inputs WHERE operation_id=?1",
        id
    )
    .fetch_one(&store.pool)
    .await
    .expect("fetch one succeeds");
    let captured = serde_json::from_str::<ApplicationTemplate>(&json)
        .expect("the value parses")
        .with_files(
            [("notes.yaml".to_owned(), "notes: true".to_owned())]
                .into_iter()
                .collect(),
        )
        .expect("with files succeeds")
        .canonical_json()
        .expect("canonical json succeeds");
    sqlx::query!(
        "UPDATE deployment_inputs SET application_json=?1,fetched_json=?1 WHERE operation_id=?2",
        captured,
        id
    )
    .execute(&store.pool)
    .await
    .expect("execute succeeds");
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    let saved = store
        .application(&application)
        .await
        .expect("application succeeds")
        .application;
    assert_eq!(saved.spec().services[0].replicas, 3.into());
}

/// Each environment's hostname reservations and rename checks read its
/// own last fetched manifest, so a fetch for one never changes the other.
#[tokio::test]
async fn environments_reserve_and_rename_by_their_own_fetched_manifest() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application,
                name: piqueld_core::EnvironmentName::parse("staging")
                    .expect("the environment name parses"),
                source: Some(crate::SourceChoice::Branch(
                    piqueld_core::TrackedBranch::new("release".into(), None)
                        .expect("the value is valid"),
                )),
            },
            None,
            true,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("environment")
    };
    let reserved = async |environment: &EnvironmentId| {
        let id = environment.as_str();
        sqlx::query_scalar!(
            "SELECT hostname FROM hostname_reservations WHERE environment_id=?1 ORDER BY hostname",
            id
        )
        .fetch_all(&store.pool)
        .await
        .expect("fetch all succeeds")
    };
    let service = token_spec(false);
    fetch(
        &store,
        &production,
        &format!("{service}\n[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=80\n[spec.environments.production.variables]\nlevel='debug'"),
    )
    .await;
    assert_eq!(reserved(&production).await, ["notes.example.com"]);
    assert_eq!(reserved(&staging.id).await, Vec::<String>::new());
    let before = store
        .get(&production)
        .await
        .expect("the environment exists")
        .own;
    fetch(
        &store,
        &staging.id,
        &format!(
            "{service}\n[[spec.routes]]\nhostname='staging.example.com'\nservice='web'\nport=80"
        ),
    )
    .await;
    assert_eq!(reserved(&production).await, ["notes.example.com"]);
    assert_eq!(reserved(&staging.id).await, ["staging.example.com"]);
    assert_eq!(
        store
            .get(&production)
            .await
            .expect("the environment exists")
            .own,
        before
    );
    let rename = |id: &EnvironmentId| Mutation::RenameEnvironment {
        id: id.clone(),
        name: piqueld_core::EnvironmentName::parse("qa").expect("the environment name parses"),
    };
    assert!(matches!(
        store.force(rename(&production)).await,
        Err(StoreError::Environment(EnvironmentError::Configured { .. }))
    ));
    store
        .force(rename(&staging.id))
        .await
        .expect("force succeeds");
}

/// Saves the repository-backed application `notes` (see `connection`).
async fn saved_connection(store: &Store) -> ApplicationId {
    let saved = store.save_template(connection(), false).await;
    ApplicationId::parse(&saved.application_id).expect("the application id parses")
}

/// A manifest with the image service `web` that declares the generated
/// secret `token` when `declared`.
fn token_spec(declared: bool) -> String {
    let secret = "[[spec.secrets]]\nname='token'\n[spec.secrets.generate]\ntype='random'\nbytes=16\nencoding='hex'\n";
    format!(
        "{}[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'",
        if declared { secret } else { "" }
    )
}

/// Fetching a manifest from the branch `spec.manifest` names converges
/// the environments it declares, including the fetching one, whose own
/// deployment is past building; other branches never do.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The scenario reads as one story, and its expect messages wrapped it past the limit."
)]
async fn fetches_of_the_connection_branch_converge_declared_environments() {
    use piqueld_core::{EnvironmentSource, TrackedBranch};
    let (_directory, store) = Store::temporary().await;
    let polled = template(
        "notes",
        "[spec.manifest]\npath='app.toml'\nsync={mode='poll'}\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'",
    );
    let saved = store.save_template(polled, false).await;
    let application =
        ApplicationId::parse(&saved.application_id).expect("the application id parses");
    let production = store.production(&application).await;
    let declared = format!(
        "[spec.environments.staging]\nsync=true\n[spec.environments.production]\npromote_from='staging'\n[spec.environments.canary]\nbranch='next'\n{}",
        token_spec(false)
    );
    fetch(&store, &production, &declared).await;
    let environments = store
        .environments(&application)
        .await
        .expect("environments succeeds");
    let named = |name: &str| {
        environments
            .iter()
            .find(|environment| environment.name.as_str() == name)
            .expect("the item exists")
            .clone()
    };
    let (staging, canary) = (named("staging"), named("canary"));
    assert_eq!(
        staging.source,
        EnvironmentSource::Branch(
            TrackedBranch::new("main".into(), None).expect("the value is valid")
        )
    );
    assert!(staging.sync);
    assert_eq!(
        canary.source,
        EnvironmentSource::Branch(
            TrackedBranch::new("next".into(), None).expect("the value is valid")
        )
    );
    assert_eq!(
        named("production").source.promoted_from(),
        Some(&staging.id)
    );

    fetch(&store, &canary.id, &token_spec(false)).await;
    assert_eq!(
        store
            .environments(&application)
            .await
            .expect("environments succeeds")
            .len(),
        3
    );
    let id = staging.id.as_str();
    let first = "a".repeat(40);
    sqlx::query!(
        "UPDATE environments SET synced_commit=?1,synced_at_ms=1 WHERE id=?2",
        first,
        id
    )
    .execute(&store.pool)
    .await
    .expect("execute succeeds");
    let (MutationResponse::Synced(crate::SyncOutcome::Deployed(synced)), _) = store
        .accept(
            crate::Actor::System(piqueld_core::sync::SystemActor::SyncPoll),
            Mutation::Sync(crate::ListedHead {
                id: staging.id.clone(),
                repository: "https://example.com/notes.git".into(),
                branch: piqueld_core::GitBranch::parse("main").expect("the git branch parses"),
                head: "b".repeat(40),
                since: store
                    .get(&staging.id)
                    .await
                    .expect("the environment exists")
                    .environment
                    .synced,
            }),
            None,
            false,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("sync deployment expected")
    };
    let (op, target) = prepared(
        &store,
        &staging.id,
        &synced.operation_id,
        &format!("{declared}\n[spec.environments.qa]\n"),
    )
    .await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    assert_eq!(
        store
            .environments(&application)
            .await
            .expect("environments succeeds")
            .len(),
        4
    );
    fetch(
        &store,
        &staging.id,
        &format!(
            "[spec.environments.staging]\n[spec.environments.production]\npromote_from='staging'\n{}",
            token_spec(false)
        ),
    )
    .await;
    let environments = store
        .environments(&application)
        .await
        .expect("environments succeeds");
    assert_eq!(environments.len(), 4);
    assert!(!environments.iter().any(|environment| environment.sync));
}

/// A fetch that changes the saved manifest resolves its environment at
/// the revision it saved, unless it changes that environment's source.
#[tokio::test]
async fn fetches_resolve_their_environment_at_the_revision_they_save() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (op, target) = fetched(&store, &production, &token_spec(false)).await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    store
        .publish_prepared(&op)
        .await
        .expect("publish prepared succeeds");
    let current = store
        .get(&production)
        .await
        .expect("the environment exists");
    assert_eq!(current.application.generation, op.generation + 1);
    assert_eq!(
        current.environment.resolved_generation,
        Some(current.application.generation)
    );
    let moved = format!(
        "[spec.environments.production]\nbranch='release'\n{}",
        token_spec(false)
    );
    let (op, target) = fetched(&store, &production, &moved).await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    store
        .publish_prepared(&op)
        .await
        .expect("publish prepared succeeds");
    let current = store
        .get(&production)
        .await
        .expect("the environment exists");
    assert_ne!(
        current.environment.resolved_generation,
        Some(current.application.generation)
    );
}

/// A fetch that changes an environment's source leaves it unresolved, as
/// changing it with `env branch` does, and, like any fetch changing the
/// saved manifest, advances the revision.
#[tokio::test]
async fn fetched_source_changes_leave_environments_unresolved() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let canary = |branch: &str| {
        format!(
            "[spec.environments.canary]\nbranch='{branch}'\n{}",
            token_spec(false)
        )
    };
    fetch(&store, &production, &canary("next")).await;
    let id = store
        .environments(&application)
        .await
        .expect("environments succeeds")
        .into_iter()
        .find(|environment| environment.name.as_str() == "canary")
        .expect("the item exists")
        .id;
    let canary_id = id.as_str();
    sqlx::query!(
        "UPDATE environments SET resolved_generation=(SELECT generation FROM applications) WHERE id=?1",
        canary_id
    )
    .execute(&store.pool)
    .await
    .expect("execute succeeds");
    let generation = store
        .get(&id)
        .await
        .expect("the environment exists")
        .application
        .generation;
    fetch(&store, &production, &canary("release")).await;
    let current = store.get(&id).await.expect("the environment exists");
    assert_eq!(current.environment.resolved_generation, None);
    assert_eq!(current.application.generation, generation + 1);

    store
        .accept(Daemon, Mutation::deploy(id.clone()), None, false, None)
        .await
        .expect("accept succeeds");
    fetch(&store, &production, &canary("beta")).await;
    assert_eq!(
        store
            .get(&id)
            .await
            .expect("the environment exists")
            .application
            .generation,
        generation + 2
    );
}

/// The saved manifest's declarations hold their names even for an
/// environment whose own fetched manifest, from another branch, has no
/// block: renaming it would leave the declaration to recreate it.
#[tokio::test]
async fn declared_environments_keep_their_names() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    fetch(
        &store,
        &production,
        &format!(
            "[spec.environments.staging]\nbranch='release'\n{}",
            token_spec(false)
        ),
    )
    .await;
    let staging = store
        .environments(&application)
        .await
        .expect("environments succeeds")
        .into_iter()
        .find(|environment| environment.name.as_str() == "staging")
        .expect("the item exists")
        .id;
    fetch(&store, &staging, &token_spec(false)).await;
    let rename = Mutation::RenameEnvironment {
        id: staging,
        name: piqueld_core::EnvironmentName::parse("qa").expect("the environment name parses"),
    };
    assert!(matches!(
        store.force(rename).await,
        Err(StoreError::Environment(EnvironmentError::Declared { environment })) if environment.as_str() == "staging"
    ));
}

/// A secret stored after a manifest was fetched but before it is accepted
/// may not share a name with one of its declarations.
#[tokio::test]
async fn accepting_a_fetched_manifest_rejects_declarations_stored_meanwhile() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let production = store.production(&application).await;
    let (op, target) = fetched(&store, &production, &token_spec(true)).await;
    store
        .put_stored_secret(Daemon, &application, "token", 0, b"value".to_vec(), None)
        .await
        .expect("put stored secret succeeds");
    let Err(StoreError::Validation(errors)) = store.save_prepared(&op, &target).await else {
        panic!("secret name conflict expected")
    };
    assert_eq!(
        errors.0[0].code,
        piqueld_core::ApiErrorCode::SecretNameConflict
    );
    assert!(
        store
            .get(&production)
            .await
            .expect("the environment exists")
            .manifest()
            .is_none()
    );
}

/// A name an environment's last fetched manifest declares cannot be
/// stored, even once another environment's fetch replaced the saved one.
#[tokio::test]
async fn fetched_declarations_of_every_environment_reserve_their_names() {
    let (_directory, store) = Store::temporary().await;
    let application = saved_connection(&store).await;
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: application.clone(),
                name: piqueld_core::EnvironmentName::parse("staging")
                    .expect("the environment name parses"),
                source: Some(crate::SourceChoice::Branch(
                    piqueld_core::TrackedBranch::new("release".into(), None)
                        .expect("the value is valid"),
                )),
            },
            None,
            true,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("environment")
    };
    let production = store.production(&application).await;
    fetch(&store, &production, &token_spec(true)).await;
    fetch(&store, &staging.id, &token_spec(false)).await;
    let Err(StoreError::Validation(errors)) = store
        .put_stored_secret(Daemon, &application, "token", 0, b"value".to_vec(), None)
        .await
    else {
        panic!("secret name conflict expected")
    };
    assert_eq!(
        errors.0[0].code,
        piqueld_core::ApiErrorCode::SecretNameConflict
    );
}
