use super::*;
use crate::testing::template;
use crate::{Actor, Mutation, MutationResponse};
use piqueld_core::sync::{RepositorySync, SyncedHead};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// Saves the repository-backed `notes`, polling `main`, and returns its
/// only environment, opted in and never deployed.
async fn notes(store: &Store) -> EnvironmentId {
    let template = template(
        "notes",
        "[spec.manifest]\npath='app.toml'\nsync={mode='poll'}\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n",
    );
    let saved = store.save_template(template, false).await;
    let application = ApplicationId::parse(saved.application_id).unwrap();
    let id = store.environments(&application).await.unwrap()[0]
        .id
        .clone();
    let opt_in = Mutation::SetSync {
        id: id.clone(),
        enabled: true,
    };
    store
        .accept(Actor::Daemon, opt_in, None, false, None)
        .await
        .unwrap();
    id
}

/// Submits sync's mutation for `head` at `main`, as `sync:poll`.
async fn sync(store: &Store, id: &EnvironmentId, head: &str) -> SyncOutcome {
    listed(store, id, "https://example.com/notes.git", head).await
}

/// Submits sync's mutation for `head` at `main` of `repository`, listed
/// since the current head of its last deployment.
async fn listed(store: &Store, id: &EnvironmentId, repository: &str, head: &str) -> SyncOutcome {
    let since = store.get(id).await.unwrap().environment.synced;
    submit(store, id, repository, head, since).await
}

/// Submits sync's mutation for `head` at `main` of `repository`, listed
/// since `since`.
async fn submit(
    store: &Store,
    id: &EnvironmentId,
    repository: &str,
    head: &str,
    since: Option<SyncedHead>,
) -> SyncOutcome {
    let mutation = Mutation::Sync(ListedHead {
        id: id.clone(),
        repository: repository.into(),
        branch: piqueld_core::GitBranch::parse("main").unwrap(),
        head: head.into(),
        since,
    });
    let actor = Actor::System(piqueld_core::sync::SystemActor::SyncPoll);
    match store
        .accept(actor, mutation, None, false, None)
        .await
        .unwrap()
    {
        (MutationResponse::Synced(outcome), _) => outcome,
        _ => panic!("synced"),
    }
}

async fn operations(store: &Store, id: &EnvironmentId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE environment_id=?1")
        .bind(id.as_str())
        .fetch_one(&store.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_moved_head_deploys_once_by_its_actor_and_pushes_coalesce_while_deploying() {
    let (_temp, store) = Store::temporary().await;
    let id = notes(&store).await;
    store.deployed_at(&id, A).await;

    let SyncOutcome::Deployed(deployed) = sync(&store, &id, B).await else {
        panic!("deployed")
    };
    let operation = store.operation(&deployed.operation_id).await.unwrap();
    let input = store.deployment_input(&operation).await.unwrap().unwrap();
    let fetched = input.template.spec().manifest.clone().unwrap().repository;
    assert_eq!(
        (fetched.branch.as_str(), fetched.commit.as_deref()),
        ("main", Some(B))
    );
    let actors: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT kind,actor_system FROM events WHERE operation_id=?1 ORDER BY id")
            .bind(&deployed.operation_id)
            .fetch_all(&store.pool)
            .await
            .unwrap();
    assert!(actors.iter().any(|(kind, _)| kind == "branch_synced"));
    assert!(
        actors
            .iter()
            .all(|(_, actor)| actor.as_deref() == Some("sync:poll")),
        "{actors:?}"
    );
    let attributed: Option<String> =
        sqlx::query_scalar("SELECT actor_system FROM operations WHERE id=?1")
            .bind(&deployed.operation_id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(attributed.as_deref(), Some("sync:poll"));
    assert_eq!(
        store
            .get(&id)
            .await
            .unwrap()
            .environment
            .synced
            .unwrap()
            .commit,
        Some(B.into())
    );

    assert!(matches!(sync(&store, &id, B).await, SyncOutcome::Busy));
    assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Busy));
    assert_eq!(operations(&store, &id).await, 2);

    sqlx::query("UPDATE operations SET state='running',error_code='image_pull_failed',error_message='failed' WHERE id=?1")
        .bind(&deployed.operation_id)
        .execute(&store.pool)
        .await
        .unwrap();
    let SyncOutcome::Deployed(retrying) = sync(&store, &id, C).await else {
        panic!("deployed")
    };
    assert_eq!(
        store.operation(&deployed.operation_id).await.unwrap().state,
        piqueld_core::OperationState::Superseded
    );

    sqlx::query("UPDATE operations SET state='running',error_code='image_pull_failed',error_message='failed' WHERE id=?1")
        .bind(&retrying.operation_id)
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(matches!(
        sync(&store, &id, A).await,
        SyncOutcome::Deployed(_)
    ));
    assert_eq!(
        store.operation(&retrying.operation_id).await.unwrap().state,
        piqueld_core::OperationState::Superseded
    );
}

#[tokio::test]
async fn a_rollback_after_a_manual_deployment_deploys_the_branch_head_again() {
    let (_temp, store) = Store::temporary().await;
    let id = notes(&store).await;
    store.deployed_at(&id, A).await;
    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
    let before = store.get(&id).await.unwrap().environment.synced;
    store.deployed_at(&id, A).await;
    let stale = submit(&store, &id, "https://example.com/notes.git", B, before).await;
    assert!(matches!(stale, SyncOutcome::Busy));
    store.deployed_at(&id, C).await;
    assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
    assert!(matches!(
        sync(&store, &id, A).await,
        SyncOutcome::Deployed(_)
    ));
    sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE environment_id=?1 AND state='requested'")
        .bind(id.as_str())
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
    let deploy = Mutation::deploy(id.clone());
    store.force(deploy).await.unwrap();
    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Busy));
}

#[tokio::test]
async fn nothing_follows_before_its_first_deployment_nor_while_pinned_or_opted_out() {
    let (_temp, store) = Store::temporary().await;
    let id = notes(&store).await;

    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
    assert_eq!(store.get(&id).await.unwrap().environment.synced, None);
    let deploy = Mutation::deploy(id.clone());
    store.force(deploy).await.unwrap();
    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Busy));
    sqlx::query("UPDATE operations SET state='failed',finished_at_ms=1,error_code='manifest_invalid',error_message='invalid' WHERE environment_id=?1")
        .bind(id.as_str())
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
    store.deployed_at(&id, B).await;
    assert!(matches!(sync(&store, &id, B).await, SyncOutcome::Current));
    let elsewhere = listed(&store, &id, "https://example.com/fork.git", C).await;
    assert!(matches!(elsewhere, SyncOutcome::Current));
    assert_eq!(operations(&store, &id).await, 2);

    let opt_out = Mutation::SetSync {
        id: id.clone(),
        enabled: false,
    };
    store
        .accept(Actor::Daemon, opt_out, None, false, None)
        .await
        .unwrap();
    assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
    let opt_in = Mutation::SetSync {
        id: id.clone(),
        enabled: true,
    };
    store
        .accept(Actor::Daemon, opt_in, None, false, None)
        .await
        .unwrap();
    let pin = Mutation::SetBranch {
        id: id.clone(),
        branch: piqueld_core::TrackedBranch::new("main".into(), Some(B.into())).unwrap(),
    };
    store.force(pin).await.unwrap();
    assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
    let unpin = Mutation::SetBranch {
        id: id.clone(),
        branch: piqueld_core::TrackedBranch::new("main".into(), None).unwrap(),
    };
    store.force(unpin).await.unwrap();
    assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
    assert_eq!(operations(&store, &id).await, 2);
}

#[tokio::test]
async fn only_turning_sync_on_or_opting_in_needs_the_permission_to_deploy() {
    use piqueld_core::access::Scope;
    use piqueld_core::edit::ApplicationEdit;
    let (_temp, store) = Store::temporary().await;
    let id = notes(&store).await;
    let application = store.get(&id).await.unwrap().environment.application_id;
    let mut grants = piqueld_core::access::Grants::default();
    grants
        .grant(
            Permission::App(AppPermission::Write),
            &Scope::one(application.clone()),
        )
        .unwrap();
    let alice = store.seed_caller("alice", &grants).await;
    let edit = |edit| Mutation::Edit {
        id: application.clone(),
        edit: Box::new(edit),
        deploy: false,
    };
    let opt_in = || Mutation::SetSync {
        id: id.clone(),
        enabled: true,
    };
    let poll = RepositorySync::Poll {
        interval_seconds: 60,
    };
    let refused = |result: Result<_, StoreError>| {
        assert!(
            matches!(result, Err(StoreError::Denied(_))),
            "{:?}",
            result.err()
        );
    };
    refused(store.accept(alice, opt_in(), None, false, None).await);
    let create = Mutation::CreateEnvironment {
        application: application.clone(),
        name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
        source: None,
    };
    let branch = Mutation::SetBranch {
        id: id.clone(),
        branch: piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
    };
    let url = edit(ApplicationEdit::RepositoryUrl(
        "https://example.com/fork.git".into(),
    ));
    for mutation in [
        create,
        branch,
        url,
        edit(ApplicationEdit::RepositorySync(poll)),
    ] {
        store
            .accept(alice, mutation, None, true, None)
            .await
            .unwrap();
    }
    let off = edit(ApplicationEdit::RepositorySync(RepositorySync::Off));
    store.accept(alice, off, None, true, None).await.unwrap();
    refused(
        store
            .accept(
                alice,
                edit(ApplicationEdit::RepositorySync(poll)),
                None,
                true,
                None,
            )
            .await,
    );
    store
        .accept(alice, opt_in(), None, false, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_deployment_records_its_head_only_while_it_follows_that_branch_and_repository() {
    let (_temp, store) = Store::temporary().await;
    let id = notes(&store).await;
    store.deployed_at(&id, A).await;
    let record = |url: &str, branch: &str| {
        let head = BranchHead {
            url: url.into(),
            branch: branch.into(),
            commit: Some(C.into()),
        };
        let store = &store;
        let id = id.clone();
        async move {
            let (_writer, mut tx) = store.begin_immediate().await.unwrap();
            Store::record_branch_head_on(&mut tx, &id, &head, 1)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    record("https://example.com/notes.git", "release").await;
    record("https://example.com/fork.git", "main").await;
    assert_eq!(
        store
            .get(&id)
            .await
            .unwrap()
            .environment
            .synced
            .unwrap()
            .commit,
        Some(A.into())
    );
    record("https://example.com/notes.git", "main").await;
    assert_eq!(
        store
            .get(&id)
            .await
            .unwrap()
            .environment
            .synced
            .unwrap()
            .commit,
        Some(C.into())
    );
    let deploy = Mutation::deploy(id.clone());
    let (MutationResponse::Operation(queued), _) = store
        .accept(Actor::Daemon, deploy, None, true, None)
        .await
        .unwrap()
    else {
        panic!("operation")
    };
    let release = Mutation::SetBranch {
        id: id.clone(),
        branch: piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
    };
    store.force(release).await.unwrap();
    let queued = store.operation(&queued.operation_id).await.unwrap();
    let input = store.deployment_input(&queued).await.unwrap().unwrap();
    assert_eq!(input.tracked_branch.as_deref(), Some("main"));
}

/// Deploys a local snapshot to the preview of `main` in slot `agent` of
/// `application`, and finishes it as the controller would, having found
/// `main` at `head`, or absent. Returns the preview.
async fn snapshot_deployed(
    store: &Store,
    application: &ApplicationId,
    head: Option<&str>,
) -> EnvironmentId {
    let mutation = Mutation::Preview(crate::PreviewMutation::Local {
        application: application.clone(),
        branch: piqueld_core::GitBranch::parse("main").unwrap(),
        slot: Some(piqueld_core::PreviewSlot::parse("agent").unwrap()),
        repository: "https://example.com/notes.git".into(),
        snapshot: piqueld_core::api::LocalSnapshot {
            commit: C.into(),
            base: None,
        },
    });
    let (MutationResponse::Preview(created), _) = store
        .accept(Actor::Daemon, mutation, None, false, None)
        .await
        .unwrap()
    else {
        panic!("preview")
    };
    let (id, operation) = (created.preview.id, created.operation.operation_id);
    let (_writer, mut tx) = store.begin_immediate().await.unwrap();
    sqlx::query(
        "UPDATE deployment_inputs SET fetched=1,repository_commit=?1 WHERE operation_id=?2",
    )
    .bind(C)
    .bind(&operation)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE id=?1")
        .bind(&operation)
        .execute(&mut *tx)
        .await
        .unwrap();
    let head = BranchHead {
        url: "https://example.com/notes.git".into(),
        branch: "main".into(),
        commit: head.map(str::to_owned),
    };
    Store::record_branch_head_on(&mut tx, &id, &head, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

/// A local snapshot differs from its branch head, which sync must not
/// take for a move: it stays until the branch really moves, or, for a
/// branch never pushed, until it is pushed.
#[tokio::test]
async fn local_snapshots_stay_until_their_branch_moves_or_is_first_pushed() {
    let (_temp, store) = Store::temporary().await;
    let environment = notes(&store).await;
    let application = store
        .get(&environment)
        .await
        .unwrap()
        .environment
        .application_id;

    let preview = snapshot_deployed(&store, &application, Some(A)).await;
    assert!(matches!(
        sync(&store, &preview, A).await,
        SyncOutcome::Current
    ));
    assert_eq!(operations(&store, &preview).await, 1);
    let SyncOutcome::Deployed(pushed) = sync(&store, &preview, B).await else {
        panic!("deployed")
    };
    assert_eq!(
        store
            .preview_origins(&preview)
            .await
            .unwrap()
            .latest
            .unwrap(),
        piqueld_core::api::DeploymentOrigin::Build
    );
    sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE id=?1")
        .bind(&pushed.operation_id)
        .execute(&store.pool)
        .await
        .unwrap();

    let preview = snapshot_deployed(&store, &application, None).await;
    let synced = store.get(&preview).await.unwrap().environment;
    assert_eq!(synced.synced.as_ref().unwrap().commit, None);
    assert_eq!(
        synced.sync_state(
            store
                .application(&application)
                .await
                .unwrap()
                .application
                .spec()
                .manifest
                .as_ref()
        ),
        piqueld_core::sync::SyncState::Following
    );
    assert!(matches!(
        sync(&store, &preview, B).await,
        SyncOutcome::Deployed(_)
    ));
    assert_eq!(
        store
            .get(&preview)
            .await
            .unwrap()
            .environment
            .synced
            .unwrap()
            .commit
            .as_deref(),
        Some(B)
    );
}
