use super::*;
use crate::OperationState;
use piqueld_core::manifest::ApplicationTemplate;

#[tokio::test]
async fn terminal_job_failure_keeps_cleanup_atomic_and_retained() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db");
    let store = Store::open_unsynced(&path).await.unwrap();
    let application = piqueld_core::parse_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='jobs'\n[spec]",
    )
    .unwrap()
    .normalize(piqueld_core::ApplicationId::parse("app-jobs").unwrap());
    let operation = store
        .save_application(&ApplicationTemplate::from(&application), None, None)
        .await
        .unwrap();
    store.mark_running(&operation.id).await;

    sqlx::query("CREATE TRIGGER reject_cleanup BEFORE INSERT ON job_cleanup BEGIN SELECT RAISE(FAIL,'cleanup storage unavailable'); END")
        .execute(&store.pool).await.unwrap();
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
        store.operation(&operation.id).await.unwrap().state,
        OperationState::Running
    );
    sqlx::query("DROP TRIGGER reject_cleanup")
        .execute(&store.pool)
        .await
        .unwrap();
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("job_timeout", "migration timed out")),
        )
        .await
        .unwrap();
    drop(store);

    let store = Store::open_unsynced(&path).await.unwrap();
    assert_eq!(
        store
            .pending_job_cleanup(&operation.environment_id)
            .await
            .unwrap(),
        std::slice::from_ref(&operation.id)
    );
    store
        .save_application(
            &ApplicationTemplate::from(&application),
            None,
            Some(operation.generation),
        )
        .await
        .unwrap();
    sqlx::query("DELETE FROM deployments WHERE id=?1")
        .bind(&operation.id)
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 0);
    assert_eq!(
        store
            .pending_job_cleanup(&operation.environment_id)
            .await
            .unwrap(),
        std::slice::from_ref(&operation.id)
    );
    store.finish_job_cleanup(&operation.id).await.unwrap();
    assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 1);
}
