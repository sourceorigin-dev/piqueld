use super::*;
use crate::OperationState;
use piqueld_core::manifest::ApplicationTemplate;

#[tokio::test]
async fn terminal_job_failure_keeps_cleanup_atomic_and_retained() {
    let directory = tempfile::tempdir().expect("tempdir succeeds");
    let path = directory.path().join("db");
    let store = Store::open_unsynced(&path)
        .await
        .expect("open unsynced succeeds");
    let application = piqueld_core::parse_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='jobs'\n[spec]",
    )
    .expect("parse toml succeeds")
    .normalize(piqueld_core::ApplicationId::parse("app-jobs").expect("the application id parses"));
    let operation = store
        .save_application(&ApplicationTemplate::from(&application), None, None)
        .await
        .expect("save application succeeds");
    store.mark_running(&operation.id).await;

    sqlx::query("CREATE TRIGGER reject_cleanup BEFORE INSERT ON job_cleanup BEGIN SELECT RAISE(FAIL,'cleanup storage unavailable'); END")
        .execute(&store.pool).await.expect("execute succeeds");
    assert!(
        store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("job_timeout", "migration timed out"))
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .operation(&operation.id)
            .await
            .expect("operation succeeds")
            .state,
        OperationState::Running
    );
    sqlx::query("DROP TRIGGER reject_cleanup")
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("job_timeout", "migration timed out")),
        )
        .await
        .expect("transition operation succeeds");
    drop(store);

    let store = Store::open_unsynced(&path)
        .await
        .expect("open unsynced succeeds");
    assert_eq!(
        store
            .pending_job_cleanup(&operation.environment_id)
            .await
            .expect("pending job cleanup succeeds"),
        std::slice::from_ref(&operation.id)
    );
    store
        .save_application(
            &ApplicationTemplate::from(&application),
            None,
            Some(operation.generation),
        )
        .await
        .expect("save application succeeds");
    sqlx::query("DELETE FROM deployments WHERE id=?1")
        .bind(&operation.id)
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    assert_eq!(
        store
            .prune_finished_operations(i64::MAX)
            .await
            .expect("prune finished operations succeeds"),
        0
    );
    assert_eq!(
        store
            .pending_job_cleanup(&operation.environment_id)
            .await
            .expect("pending job cleanup succeeds"),
        std::slice::from_ref(&operation.id)
    );
    store
        .finish_job_cleanup(&operation.id)
        .await
        .expect("finish job cleanup succeeds");
    assert_eq!(
        store
            .prune_finished_operations(i64::MAX)
            .await
            .expect("prune finished operations succeeds"),
        1
    );
}
