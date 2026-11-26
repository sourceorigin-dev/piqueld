use super::*;
use crate::Actor::Daemon;
use crate::OperationFailure;
use crate::{Mutation, MutationResponse};
use piqueld_core::observability::DiagnosticCode;
use piqueld_core::{ApplicationState, OperationState};

fn empty() -> ApplicationTemplate {
    crate::testing::template("empty", "[spec]").normalize(
        piqueld_core::ApplicationId::parse("app-empty-test").expect("the application id parses"),
    )
}

async fn save(store: &Store, app: ApplicationTemplate, generation: u64) -> SavedApplication {
    let (MutationResponse::Saved(saved), wake) = store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(app),
                expected_application_id: None,
                deploy: false,
            },
            Some(generation),
            false,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("saved response")
    };
    assert!(!wake);
    saved
}

/// Requests a deployment of `id`, returning it and whether the controller should wake.
async fn deploy(
    store: &Store,
    id: &EnvironmentId,
    generation: u64,
    key: Option<&str>,
) -> (piqueld_core::api::AcceptedOperation, bool) {
    let (MutationResponse::Operation(operation), wake) = store
        .accept(
            Daemon,
            Mutation::deploy(id.clone()),
            Some(generation),
            false,
            key,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("operation response")
    };
    (operation, wake)
}

#[tokio::test]
async fn save_and_deploy_accepts_saved_only_configuration_but_cannot_reverse_deletion() {
    let (_temp, store) = Store::temporary().await;
    let saved = save(&store, empty(), 0).await;
    let id = store.saved_production(&saved).await;
    let application = store
        .get(&id)
        .await
        .expect("the environment exists")
        .application
        .application;
    let (MutationResponse::Saved(deployed), wake) = store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(application.clone()),
                expected_application_id: Some(saved.application_id.clone()),
                deploy: true,
            },
            Some(saved.generation),
            false,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("saved deployment")
    };
    assert!(wake);
    let operation_id = deployed.operation_id.expect("operation id is present");
    assert_eq!(
        store
            .deployment_snapshot(&operation_id)
            .await
            .expect("deployment snapshot succeeds")
            .template,
        application
    );
    let (MutationResponse::Deleted(deletion), _) = store
        .accept(
            Daemon,
            Mutation::DeleteApplication {
                id: application.id().clone(),
                environments: Vec::new(),
            },
            Some(deployed.generation),
            false,
            None,
        )
        .await
        .expect("accept succeeds")
    else {
        panic!("deleted")
    };
    for deploy in [false, true] {
        assert!(matches!(
            store
                .accept(
                    Daemon,
                    Mutation::Save {
                        application: Box::new(application.clone()),
                        expected_application_id: Some(saved.application_id.clone()),
                        deploy,
                    },
                    Some(deletion.generation),
                    false,
                    None,
                )
                .await,
            Err(StoreError::IllegalTransition)
        ));
    }
    assert!(matches!(
        store
            .save_application(&application, None, Some(deletion.generation))
            .await,
        Err(StoreError::IllegalTransition)
    ));
    let current = store.get(&id).await.expect("the environment exists");
    assert!(current.delete_intent() && current.application.delete_intent);
    assert_eq!(current.application.generation, deletion.generation);
    assert_eq!(
        store
            .latest_operation_for_environment(&id)
            .await
            .expect("latest operation for environment returns a value")
            .expect("the value is present")
            .id,
        deletion.operations[0].operation_id
    );
}

#[tokio::test]
async fn saved_changes_cannot_enter_deployments_or_retries_after_restart() {
    let temp = tempfile::tempdir().expect("tempdir succeeds");
    let path = temp.path().join("db");
    let store = Store::open_unsynced(&path)
        .await
        .expect("open unsynced succeeds");
    let saved = save(&store, empty(), 0).await;
    let id = store.saved_production(&saved).await;
    assert_eq!(
        store.status(&id).await.expect("status succeeds").state,
        ApplicationState::NotDeployed
    );
    assert!(
        store
            .latest_operation_for_environment(&id)
            .await
            .expect("latest operation for environment succeeds")
            .is_none()
    );
    let (first, wake) = deploy(&store, &id, saved.generation, Some("deploy-once")).await;
    assert!(wake);
    let original = store
        .deployment_snapshot(&first.operation_id)
        .await
        .expect("deployment snapshot succeeds")
        .template;
    store.mark_running(&first.operation_id).await;
    let mut edited = original.to_manifest();
    edited.spec.volumes.push(piqueld_core::manifest::Volume {
        name: "later".into(),
    });
    let edited = edited
        .validate_template()
        .expect("validate template succeeds")
        .normalize(original.id().clone());
    let changed = save(&store, edited, 1).await;
    assert_eq!(changed.generation, 2);
    drop(store);
    let store = Store::open_unsynced(&path)
        .await
        .expect("open unsynced succeeds");
    store
        .recover_interrupted()
        .await
        .expect("recover interrupted succeeds");
    let op = store
        .operation(&first.operation_id)
        .await
        .expect("operation succeeds");
    let attempts = store
        .deployment_attempts(&op.id, None, 100)
        .await
        .expect("deployment attempts succeeds");
    assert_eq!(attempts.items.len(), 1);
    assert_eq!(attempts.items[0].attempt, 1);
    assert_eq!(attempts.items[0].state, OperationState::Cancelled);
    assert!(attempts.items[0].finished_at_ms.is_some());
    assert_eq!(
        store
            .recover_interrupted()
            .await
            .expect("recover interrupted succeeds"),
        0
    );
    assert_eq!(op.generation, 1);
    assert_eq!(
        store
            .deployment_snapshot(&op.id)
            .await
            .expect("deployment snapshot succeeds")
            .template,
        original
    );
    let (replay, wake) = deploy(&store, &id, 1, Some("deploy-once")).await;
    assert!(!wake);
    assert_eq!(replay.operation_id, op.id);
    let (next, _) = deploy(&store, &id, 2, None).await;
    assert_ne!(next.operation_id, op.id);
    assert!(matches!(
        store.retry_operation(&op).await,
        Err(StoreError::IllegalTransition)
    ));
    assert_eq!(
        store
            .deployments(&id, None, 3)
            .await
            .expect("deployments succeeds")
            .items
            .len(),
        2
    );
    assert_eq!(
        store
            .prune_finished_operations(i64::MAX)
            .await
            .expect("prune finished operations succeeds"),
        0
    );
}

#[tokio::test]
async fn failure_survives_success_and_empty_target_has_no_resources() {
    let (_temp, store) = Store::temporary().await;
    let saved = save(&store, empty(), 0).await;
    let id = store.saved_production(&saved).await;
    let op = store
        .request_deploy(&id, Some(1))
        .await
        .expect("request deploy succeeds");
    let target = piqueld_core::compile_application(
        &store
            .deployment_snapshot(&op.id)
            .await
            .expect("deployment snapshot succeeds")
            .rendering
            .expect("rendering is present")
            .application,
        &id,
        store.instance_id().clone(),
        &piqueld_core::ResolutionSet::default(),
    )
    .expect("compile application succeeds");
    assert!(target.services.is_empty() && target.networks.is_empty() && target.volumes.is_empty());
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(&OperationFailure::new(
                DiagnosticCode::DockerUnavailable,
                "Docker is unavailable",
            )),
        )
        .await
        .expect("transition operation succeeds");
    let op = store.operation(&op.id).await.expect("operation succeeds");
    store
        .retry_operation(&op)
        .await
        .expect("retry operation succeeds");
    store.mark_running(&op.id).await;
    store
        .save_prepared(&op, &target)
        .await
        .expect("save prepared succeeds");
    store
        .publish_prepared(&op)
        .await
        .expect("publish prepared succeeds");
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .expect("transition operation succeeds");
    store
        .prune_events(i64::MAX)
        .await
        .expect("prune events succeeds");
    let history = store
        .deployment_attempts(&op.id, None, 100)
        .await
        .expect("deployment attempts succeeds");
    assert_eq!(history.items.len(), 2);
    assert_eq!(
        history.items[1].error_code.as_deref(),
        Some("docker_unavailable")
    );
    let deployments = store
        .deployments(&id, None, 3)
        .await
        .expect("deployments succeeds");
    assert!(deployments.items[0].current_target && deployments.items[0].last_successful);
}

/// Deployment IDs are monotonic only within one process, so after a
/// restart with the clock behind, a newer deployment can sort below an
/// older one: the current and last successful deployments are the
/// newest by creation time, not by ID.
#[tokio::test]
async fn current_deployment_is_the_newest_by_creation_time_not_by_id() {
    let (_temp, store) = Store::temporary().await;
    let saved = store
        .save_template(crate::testing::template("alpha", "[spec]"), true)
        .await;
    let id = store.saved_production(&saved).await;
    let alpha = saved.operation_id.expect("operation id is present");
    sqlx::query("UPDATE operations SET promoted=1,state='succeeded',finished_at_ms=created_at_ms WHERE id=?1")
        .bind(&alpha)
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    sqlx::query("UPDATE deployments SET succeeded_at_ms=created_at_ms WHERE id=?1")
        .bind(&alpha)
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    let older = format!("operation-{}", "f".repeat(32));
    sqlx::query("INSERT INTO operations(id,environment_id,kind,state,generation,promoted,created_at_ms,updated_at_ms,finished_at_ms) SELECT ?1,environment_id,kind,'succeeded',generation,1,created_at_ms-60000,created_at_ms-60000,created_at_ms-60000 FROM operations WHERE id=?2")
        .bind(&older)
        .bind(&alpha)
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    sqlx::query("INSERT INTO deployments(id,environment_id,generation,manifest_json,template_json,variables_json,created_at_ms,succeeded_at_ms) SELECT ?1,environment_id,generation,manifest_json,template_json,variables_json,created_at_ms-60000,created_at_ms-60000 FROM deployments WHERE id=?2")
        .bind(&older)
        .bind(&alpha)
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    let latest = store
        .latest_operation_for_environment(&id)
        .await
        .expect("latest operation for environment succeeds");
    assert_eq!(latest.expect("latest is present").id, alpha);
    assert_eq!(
        store
            .current_deployment(&id)
            .await
            .expect("current deployment returns a value")
            .expect("the value is present")
            .id,
        alpha
    );
    let deployments = store
        .deployments(&id, None, 2)
        .await
        .expect("deployments succeeds")
        .items;
    let flags = deployments
        .iter()
        .map(|deployment| {
            (
                deployment.operation.id.as_str(),
                deployment.current_target,
                deployment.last_successful,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        flags,
        [(older.as_str(), false, false), (alpha.as_str(), true, true)]
    );
}
