//! Runtime secrets: rotation, cleanup, missing secrets and an unusable secret key.

use super::*;

#[tokio::test]
async fn secret_rotation_requires_deploy_and_service_references_follow_pinned_versions() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    let app = harness.application_of(&first.environment_id).await;
    harness
        .store_token(&first.environment_id, 0, b"version-one")
        .await;
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployment).await;
    let target = harness.target(&first.environment_id).await;
    let old = target.secret_names["token"].clone();
    assert_eq!(
        harness.docker.secrets.lock().await[&old].value,
        b"version-one"
    );
    harness
        .store_token(&first.environment_id, 1, b"version-two")
        .await;
    harness.scan().await;
    assert_eq!(
        harness.target(&first.environment_id).await,
        target,
        "rotation alone must not update services"
    );
    let next = applications
        .deploy(&first.environment_id, None)
        .await
        .expect("deploy succeeds");
    harness.finish(&next).await;
    let target = harness.target(&first.environment_id).await;
    let new = &target.secret_names["token"];
    assert_ne!(&old, new);
    assert_eq!(
        harness.docker.secrets.lock().await[new].value,
        b"version-two"
    );
    assert_eq!(
        harness
            .docker
            .observe(&first.environment_id)
            .await
            .expect("observe succeeds")
            .services[0]
            .secrets,
        target.services[0].secrets
    );

    let clean = harness.apply(manifest(), Some(2)).await;
    harness.finish(&clean).await;
    applications
        .delete_stored_secret(Daemon, &app, "token", 2)
        .await
        .expect("the secret is deleted");
    assert!(harness.docker.secrets.lock().await.is_empty());
    assert!(
        harness
            .store
            .stored_secrets(&app)
            .await
            .expect("the stored secrets list")
            .is_empty()
    );
}

#[tokio::test]
async fn application_deletion_journals_secret_cleanup_failures() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store
        .put_secret(Daemon, &first.environment_id, "token", 0, b"value".to_vec())
        .await
        .expect("the secret is stored");
    harness
        .docker
        .fail_secret_removal
        .store(true, Ordering::SeqCst);
    let deletion = applications
        .delete(&first.environment_id, Some(1))
        .await
        .expect("delete succeeds");
    harness.scan().await;
    let events = harness.operation_events(&deletion.id).await;
    let phases = |kind: &str| {
        events
            .iter()
            .filter(|event| {
                event.kind == kind
                    && event.phase.map(piqueld_core::Phase::as_str) == Some("remove_secrets")
            })
            .count()
    };
    assert_eq!(phases("action_requested"), 1, "{events:#?}");
    assert!(events.iter().any(|event| event.kind == "action_failed"
        && event.phase.map(piqueld_core::Phase::as_str) == Some("remove_secrets")
        && event.error_code.as_deref() == Some("docker_request_failed")));
    harness
        .store
        .get(&first.environment_id)
        .await
        .expect("`get` should succeed");
}

/// A mounted secret nobody stored fails the deployment as `secret_missing`,
/// naming it, rather than as a journal failure.
#[tokio::test]
async fn missing_stored_secrets_fail_deployment_with_their_names() {
    let harness = ControllerHarness::new().await;
    let failed = harness.apply(with_token(manifest()), Some(0)).await;
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_missing"));
    assert!(
        result
            .error_message
            .as_deref()
            .expect("the value is present")
            .contains("token")
    );
}

#[tokio::test]
async fn secret_key_recovery_leaves_running_services_and_blocks_discarded_rollouts() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let initial = harness.apply(manifest(), Some(0)).await;
    harness.finish(&initial).await;
    let id = &initial.environment_id;
    harness.store_token(id, 0, b"original").await;
    let deployed = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployed).await;
    let target = harness.target(id).await;
    let original = harness.docker.observe(id).await.expect("observe succeeds");

    std::fs::remove_file(harness.database_path.with_file_name("secrets.key"))
        .expect("the file is removed");
    harness
        .store
        .recover_secret_key(Daemon)
        .await
        .expect("the secret key recovers");
    let failed = applications
        .deploy(id, None)
        .await
        .expect("deploy succeeds");
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_unavailable"));
    assert!(
        result
            .error_message
            .as_deref()
            .expect("the value is present")
            .contains("token")
    );
    assert_eq!(harness.target(id).await, target);
    assert_eq!(
        harness.docker.observe(id).await.expect("observe succeeds"),
        original
    );

    harness.store_token(id, 1, b"replacement").await;
    let replacement = applications
        .deploy(id, None)
        .await
        .expect("deploy succeeds");
    harness.finish(&replacement).await;
    let new_target = harness.target(id).await;
    assert_eq!(
        harness.docker.secrets.lock().await[&new_target.secret_names["token"]].value,
        b"replacement"
    );
    assert_eq!(
        harness.docker.secrets.lock().await[&target.secret_names["token"]].value,
        b"original",
        "running services keep their existing Docker secret"
    );
}

#[tokio::test]
async fn missing_secret_key_fails_rollout_before_docker_mutation() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store_token(&first.environment_id, 0, b"value")
        .await;
    std::fs::remove_file(harness.database_path.with_file_name("secrets.key"))
        .expect("the file is removed");
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.scan().await;
    let operation = harness.operation(&deployment.id).await;
    assert_eq!(
        operation.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert!(harness.docker.secrets.lock().await.is_empty());
    let events = harness.operation_events(&deployment.id).await;
    let service = |kind: &str| {
        events.iter().find(|event| {
            event.kind == kind
                && event.phase.map(piqueld_core::Phase::as_str) == Some("ensure_service")
        })
    };
    let failed = service("action_failed").expect("service action failed");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert_eq!(
        failed.diagnostic.as_ref().expect("as ref succeeds").causes,
        ["Secret master key: missing"]
    );
    assert!(
        service("action_requested").is_none(),
        "no Docker request is made without secret values: {events:#?}"
    );
}

impl ControllerHarness {
    /// Whether the store still records the stored-secret copy `swarm_name`.
    async fn copy_recorded(&self, swarm_name: &str) -> bool {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM application_secret_copies WHERE swarm_name=?1",
        )
        .bind(swarm_name)
        .fetch_one(&mut self.database().await)
        .await
        .expect("the row exists")
            > 0
    }

    /// Runs the controller, whose first pass prunes history, until `done`.
    async fn run_until(&self, done: impl AsyncFn() -> bool) {
        let wake = Arc::new(tokio::sync::Notify::new());
        let run = self.controller.run(
            wake,
            Some(Duration::from_secs(60)),
            CancellationToken::new(),
        );
        tokio::select! {
            result = run => panic!("the controller stopped: {result:?}"),
            found = tokio::time::timeout(Duration::from_secs(3), self.store.wait_until(done)) => {
                found.expect("the history pass finishes");
            }
        }
    }

    /// The `remove_secrets` actions of `environment` that ended as `kind`.
    async fn removals(&self, environment: &EnvironmentId, kind: &str) -> usize {
        self.store
            .events(Some(environment), None, 100)
            .await
            .expect("the events list")
            .items
            .iter()
            .filter(|event| {
                event.phase == Some(piqueld_core::Phase::RemoveSecrets) && event.kind == kind
            })
            .count()
    }
}

/// The fixture deployed, then deployed mounting stored secret `token` at its
/// first version, then deployed again once `token` was rotated.
struct Rotation {
    environment: EnvironmentId,
    /// The deployment pinning the first version.
    pinning: Operation,
    /// The deployment pinning the second version.
    next: Operation,
    /// The Docker secrets of the first and second versions.
    old: String,
    new: String,
}

impl Rotation {
    async fn new(harness: &ControllerHarness) -> Self {
        let first = harness.apply(manifest(), Some(0)).await;
        harness.finish(&first).await;
        let environment = first.environment_id;
        harness.store_token(&environment, 0, b"version-one").await;
        let pinning = harness.apply(with_token(manifest()), Some(1)).await;
        harness.finish(&pinning).await;
        let old = harness.target(&environment).await.secret_names["token"].clone();
        harness.store_token(&environment, 1, b"version-two").await;
        let next = harness
            .applications()
            .deploy(&environment, None)
            .await
            .expect("deploy succeeds");
        harness.finish(&next).await;
        let new = harness.target(&environment).await.secret_names["token"].clone();
        Self {
            environment,
            pinning,
            next,
            old,
            new,
        }
    }

    /// Asserts the old version's Docker secret is gone and the new one's
    /// remains.
    async fn assert_only_new_remains(&self, harness: &ControllerHarness) {
        let secrets = harness.docker.secrets.lock().await;
        assert!(!secrets.contains_key(&self.old) && secrets.contains_key(&self.new));
    }
}

#[tokio::test]
async fn history_passes_remove_versions_no_deployment_pins_from_docker_then_the_store() {
    let harness = ControllerHarness::new().await;
    let rotation = Rotation::new(&harness).await;
    sqlx::query("DELETE FROM operations WHERE id=?1")
        .bind(&rotation.pinning.id)
        .execute(&mut harness.database().await)
        .await
        .expect("the statement executes");

    harness
        .docker
        .fail_secret_removal
        .store(true, Ordering::SeqCst);
    harness
        .run_until(async || {
            harness
                .removals(&rotation.environment, "action_failed")
                .await
                > 0
        })
        .await;
    assert!(harness.copy_recorded(&rotation.old).await);

    harness
        .docker
        .fail_secret_removal
        .store(false, Ordering::SeqCst);
    harness
        .run_until(async || !harness.copy_recorded(&rotation.old).await)
        .await;
    rotation.assert_only_new_remains(&harness).await;
    assert!(harness.copy_recorded(&rotation.new).await);
}

/// End to end, with the daemon's own history pruning: a rotated secret's old
/// version goes once the deployments pinning it are past both history
/// limits, but not while a service still mounts it, which fails nothing.
#[tokio::test]
async fn pruned_deployments_release_old_versions_unless_a_service_still_mounts_them() {
    let harness = ControllerHarness::new().await;
    harness.retain(|settings| {
        settings.retention.deployments_per_environment = 1;
        settings.images.keep_deployments = 1;
    });
    let rotation = Rotation::new(&harness).await;
    let Rotation {
        environment, old, ..
    } = &rotation;
    sqlx::query("UPDATE operations SET finished_at_ms=0 WHERE id!=?1")
        .bind(&rotation.next.id)
        .execute(&mut harness.database().await)
        .await
        .expect("the statement executes");

    harness.docker.observed.lock().await.services[0]
        .secrets
        .push(piqueld_core::resource::SecretFile {
            secret_name: old.clone(),
            target: "/run/secrets/old".into(),
        });
    harness
        .run_until(async || harness.removals(environment, "action_succeeded").await > 0)
        .await;
    assert!(matches!(
        harness.store.operation(&rotation.pinning.id).await,
        Err(piqueld_store::StoreError::NotFound)
    ));
    assert!(harness.copy_recorded(old).await);
    assert!(harness.docker.secrets.lock().await.contains_key(old));
    assert_eq!(harness.removals(environment, "action_failed").await, 0);

    for service in &mut harness.docker.observed.lock().await.services {
        service.secrets.retain(|secret| &secret.secret_name != old);
    }
    harness
        .run_until(async || !harness.copy_recorded(old).await)
        .await;
    rotation.assert_only_new_remains(&harness).await;
}
