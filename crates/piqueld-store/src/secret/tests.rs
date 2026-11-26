use super::*;
use crate::Actor::Daemon;
use piqueld_core::manifest::ApplicationTemplate;

/// The [`prebuilt`](crate::testing::prebuilt) application these tests save.
fn application() -> NormalizedApplication {
    crate::testing::prebuilt("app-secret-test")
}

fn with_secret(app: &NormalizedApplication) -> NormalizedApplication {
    let mut manifest = app.to_manifest();
    manifest.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    manifest
        .validate()
        .expect("the manifest validates")
        .normalize(app.id().clone())
}

/// `with_secret`, with `token` declared as a generated secret.
fn with_generated_secret(app: &NormalizedApplication) -> NormalizedApplication {
    let mut manifest = with_secret(app).to_manifest();
    manifest.spec.secrets.push(SecretDeclaration {
        name: "token".into(),
        generate: piqueld_core::manifest::SecretGenerator::Random {
            bytes: 16,
            encoding: piqueld_core::manifest::SecretEncoding::Hex,
        },
    });
    manifest
        .validate()
        .expect("the manifest validates")
        .normalize(app.id().clone())
}

/// Opens a store and saves `with_secret(application())` in it with the value
/// `original` of `token`. Returns the store's directory, which holds
/// `secrets.key`, the store, the application, and the deployment saving it
/// requested.
async fn stored_token() -> (tempfile::TempDir, Store, NormalizedApplication, Operation) {
    let (temp, store) = Store::temporary().await;
    let app = with_secret(&application());
    let deployed = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"original".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    (temp, store, app, deployed)
}

/// Saves `app` over the application it names, as the daemon at revision
/// `expected`, without deploying it.
async fn save(store: &Store, app: &NormalizedApplication, expected: u64) -> Result<(), StoreError> {
    let save = crate::Mutation::Save {
        application: Box::new(ApplicationTemplate::from(app)),
        expected_application_id: Some(app.id().clone()),
        deploy: false,
    };
    store
        .accept(Daemon, save, Some(expected), false, None)
        .await?;
    Ok(())
}

#[tokio::test]
async fn captured_deployment_protects_secrets_before_pinning() {
    let (_temp, store) = Store::temporary().await;
    let app = application();
    let captured = with_secret(&app);
    let op = store
        .save_application(&ApplicationTemplate::from(&captured), None, None)
        .await
        .expect("the store can save application");
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"value".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    save(&store, &app, 1).await.expect("the store can save");
    assert!(matches!(
        store
            .begin_stored_secret_deletion(Daemon, app.id(), "token", 1)
            .await,
        Err(StoreError::SecretReferenced)
    ));
    assert_eq!(
        store
            .pin_secrets(&op, &captured)
            .await
            .expect("the store can pin secrets")
            .len(),
        1
    );
}

#[tokio::test]
async fn deletion_reservations_survive_restart_and_do_not_block_other_writes() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let path = temp.path().join("db");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    let app = application();
    let op = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let env = store.production(app.id()).await;
    store
        .put_secret(Daemon, &env, "token", 0, b"value".to_vec())
        .await
        .expect("the store can put secret");
    let deletion = store
        .begin_secret_deletion(Daemon, &env, "token", 1)
        .await
        .expect("the store can begin secret deletion");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.put_secret(Daemon, &env, "other", 0, b"unrelated".to_vec()),
    )
    .await
    .expect("the put finishes in time")
    .expect("the put succeeds");
    drop(store);
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    assert!(
        store
            .secrets(&env)
            .await
            .expect("the store can secrets")
            .iter()
            .find(|s| s.name == "token")
            .expect("the item is found")
            .deleting
    );
    assert!(matches!(
        store
            .put_secret(Daemon, &env, "token", 1, b"replacement".to_vec())
            .await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store
            .save_application(&ApplicationTemplate::from(&with_secret(&app)), None, None)
            .await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store.pin_secrets(&op, &with_secret(&app)).await,
        Err(StoreError::SecretDeleting)
    ));
    let retry = store
        .begin_secret_deletion(Daemon, &env, "token", 1)
        .await
        .expect("the store can begin secret deletion");
    assert_eq!(retry.id, deletion.id);
    assert_eq!(retry.versions, deletion.versions);
    store
        .finish_secret_deletion(Daemon, &env, "token", &retry.id)
        .await
        .expect("the store can finish secret deletion");
    store
        .put_secret(Daemon, &env, "token", 0, b"new value".to_vec())
        .await
        .expect("the store can put secret");
    store
        .finish_secret_deletion(Daemon, &env, "token", &deletion.id)
        .await
        .expect("the store can finish secret deletion");
    let names = store
        .secret_names(&env)
        .await
        .expect("the store can secret names");
    assert_eq!(
        names.len(),
        2,
        "late cleanup must preserve the recreated secret"
    );
    assert!(!names.contains(&deletion.versions[&env][0]));
}

#[tokio::test]
async fn wrong_master_key_blocks_writes_and_original_key_restores_them() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let path = temp.path().join("db");
    let key = temp.path().join("secrets.key");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let env = store.production(app.id()).await;
    store
        .put_secret(Daemon, &env, "token", 0, b"original".to_vec())
        .await
        .expect("the store can put secret");
    let original = Zeroizing::new(std::fs::read(&key).expect("the file reads"));
    let versions = store
        .secret_names(&env)
        .await
        .expect("the store can secret names");
    drop(store);
    std::fs::write(&key, [42; 32]).expect("the file writes");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    assert!(matches!(
        store
            .put_secret(Daemon, &env, "token", 1, b"wrong key".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(
        store.secrets(&env).await.expect("the store can secrets")[0].generation,
        1
    );
    std::fs::write(&key, &*original).expect("the file writes");
    store
        .put_secret(Daemon, &env, "token", 1, b"restored".to_vec())
        .await
        .expect("the store can put secret");
    assert_eq!(
        &*store
            .secret_plaintext(&env, &versions[0])
            .await
            .expect("the store can secret plaintext"),
        b"original"
    );
    let deletion = store
        .begin_secret_deletion(Daemon, &env, "token", 2)
        .await
        .expect("the store can begin secret deletion");
    store
        .finish_secret_deletion(Daemon, &env, "token", &deletion.id)
        .await
        .expect("the store can finish secret deletion");
    std::fs::remove_file(&key).expect("the file is removed");
    assert!(matches!(
        store
            .put_secret(Daemon, &env, "new", 0, b"value".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert!(
        !key.exists(),
        "the database key binding survives deletion of all values"
    );
}

#[tokio::test]
async fn retained_version_quota_rejects_writes_without_removing_values() {
    let (_temp, store) = Store::temporary().await;
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let env = store.production(app.id()).await;
    store
        .put_secret(Daemon, &env, "token", 0, b"x".to_vec())
        .await
        .expect("the store can put secret");
    Store::check_secret_quota(1, 17, 100 * 1024 * 1024 - 33)
        .expect("the store can check secret quota");
    assert!(matches!(
        Store::check_secret_quota(1, 17, 100 * 1024 * 1024 - 32),
        Err(StoreError::SecretQuota)
    ));
    sqlx::query!("WITH RECURSIVE versions(n) AS (SELECT 2 UNION ALL SELECT n+1 FROM versions WHERE n<1000) INSERT INTO secret_versions(environment_id,name,generation,swarm_name,nonce,ciphertext) SELECT s.environment_id,s.name,v.n,'fixture-'||v.n,s.nonce,s.ciphertext FROM versions v CROSS JOIN secret_versions s WHERE s.generation=1").execute(&store.pool).await.expect("the statement executes");
    assert!(matches!(
        store
            .put_secret(Daemon, &env, "token", 1, b"blocked".to_vec())
            .await,
        Err(StoreError::SecretQuota)
    ));
    assert_eq!(
        store.secrets(&env).await.expect("the store can secrets")[0].generation,
        1
    );
    assert_eq!(
        store
            .secret_names(&env)
            .await
            .expect("the store can secret names")
            .len(),
        1000
    );
    let deletion = store
        .begin_secret_deletion(Daemon, &env, "token", 1)
        .await
        .expect("the store can begin secret deletion");
    store
        .finish_secret_deletion(Daemon, &env, "token", &deletion.id)
        .await
        .expect("the store can finish secret deletion");
    store
        .put_secret(Daemon, &env, "fresh", 0, b"space freed".to_vec())
        .await
        .expect("the store can put secret");
}
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn rotation_preserves_retry_pins_and_secret_values_never_enter_snapshots() {
    let directory = tempfile::tempdir().expect("the temp dir is created");
    let path = directory.path().join("state.db");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    let op = store
        .save_application(&ApplicationTemplate::from(&application()), None, None)
        .await
        .expect("the store can save application");
    let app = with_secret(&application());
    let env = store.production(app.id()).await;
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"first-value".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    let pins = store
        .pin_secrets(&op, &app)
        .await
        .expect("the store can pin secrets");
    store
        .put_stored_secret(Daemon, app.id(), "token", 1, b"second-value".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    store
        .put_stored_secret(Daemon, app.id(), "token", 1, b"stale".to_vec(), None)
        .await
        .expect_err("`` should be rejected");
    assert_eq!(
        store
            .secret_plaintext(&env, &pins["token"])
            .await
            .expect("the store can secret plaintext")
            .as_slice(),
        b"first-value"
    );
    assert!(matches!(
        store
            .begin_stored_secret_deletion(Daemon, app.id(), "token", 2)
            .await,
        Err(StoreError::SecretReferenced)
    ));
    store
        .secret_plaintext(
            &EnvironmentId::parse("app-another").expect("the environment id is valid"),
            &pins["token"],
        )
        .await
        .expect_err("`` should be rejected");
    drop(store);
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    assert_eq!(
        store
            .pin_secrets(&op, &app)
            .await
            .expect("the store can pin secrets"),
        pins,
        "retry must use the pre-rotation pin after restart"
    );
    let snapshot = store
        .deployment_snapshot(&op.id)
        .await
        .expect("the store can deployment snapshot");
    for json in [
        serde_json::to_string(&snapshot.template),
        serde_json::to_string(
            &snapshot
                .rendering
                .expect("the deployment has a rendering")
                .application,
        ),
    ] {
        assert!(!json.expect("the value serializes").contains("first-value"));
    }
    let next = store
        .request_deploy(&env, None)
        .await
        .expect("the store can request deploy");
    let new_pins = store
        .pin_secrets(&next, &app)
        .await
        .expect("the store can pin secrets");
    assert_ne!(pins, new_pins);
    assert_eq!(
        store
            .secret_plaintext(&env, &new_pins["token"])
            .await
            .expect("the store can secret plaintext")
            .as_slice(),
        b"second-value"
    );
    std::fs::remove_file(directory.path().join("secrets.key")).expect("the file is removed");
    assert_eq!(
        store
            .stored_secrets(app.id())
            .await
            .expect("the store can stored secrets")[0]
            .metadata
            .generation,
        2,
        "metadata reads do not require the key"
    );
    store
        .put_stored_secret(Daemon, app.id(), "token", 2, b"replacement".to_vec(), None)
        .await
        .expect_err("`` should be rejected");
    assert!(!directory.path().join("secrets.key").exists());
}

#[tokio::test]
async fn lost_key_recovery_discards_values_until_replacements_are_deployed() {
    let (temp, store, app, deployed) = stored_token().await;
    let key = temp.path().join("secrets.key");
    let pinned = store
        .pin_secrets(&deployed, &app)
        .await
        .expect("the store can pin secrets");
    let env = &deployed.environment_id;
    assert!(matches!(
        store.recover_secret_key(Daemon).await,
        Err(StoreError::SecretKeyUsable)
    ));
    assert_eq!(
        &*store
            .secret_plaintext(env, &pinned["token"])
            .await
            .expect("the store can secret plaintext"),
        b"original"
    );

    std::fs::write(&key, [42; 32]).expect("the file writes");
    let recovery = store
        .recover_secret_key(Daemon)
        .await
        .expect("the store can recover secret key");
    assert_eq!(
        (
            recovery.affected_environments,
            recovery.affected_applications,
            recovery.affected_secrets,
            recovery.discarded_versions
        ),
        (0, 1, 1, 1)
    );
    assert!(!key.exists());
    let retired = std::fs::read_dir(temp.path())
        .expect("the directory reads")
        .map(|entry| entry.expect("the entry reads").path())
        .find(|path| path.to_string_lossy().contains("secrets.key.retired-"))
        .expect("the unusable key is kept aside");
    assert_eq!(std::fs::read(retired).expect("the file reads"), [42; 32]);
    assert!(matches!(
        store.recover_secret_key(Daemon).await,
        Err(StoreError::SecretKeyUsable)
    ));
    assert!(!key.exists());
    assert!(
        store
            .stored_secrets(app.id())
            .await
            .expect("the store can stored secrets")[0]
            .metadata
            .unavailable
    );
    assert!(matches!(
        store.secret_plaintext(env, &pinned["token"]).await,
        Err(StoreError::SecretUnavailable { names }) if names == "token"
    ));
    assert!(matches!(
        store.pin_secrets(&deployed, &app).await,
        Err(StoreError::SecretUnavailable { .. })
    ));

    store
        .put_stored_secret(Daemon, app.id(), "token", 1, b"replacement".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    assert!(key.exists(), "the first new value generates a key");
    let redeployed = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let pins = store
        .pin_secrets(&redeployed, &app)
        .await
        .expect("the store can pin secrets");
    assert_eq!(
        &*store
            .secret_plaintext(env, &pins["token"])
            .await
            .expect("the store can secret plaintext"),
        b"replacement"
    );
    assert!(matches!(
        store.recover_secret_key(Daemon).await,
        Err(StoreError::SecretKeyUsable)
    ));
}

/// Only a lost key is recovered: one that is merely not private is fixed on
/// the host, so recovery discards nothing.
#[tokio::test]
async fn key_recovery_keeps_values_of_a_key_that_is_not_lost() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, store, app, _) = stored_token().await;
    let key = temp.path().join("secrets.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644))
        .expect("the permissions apply");
    assert!(matches!(
        store.recover_secret_key(Daemon).await,
        Err(StoreError::SecretSource(_))
    ));
    let stored = store
        .stored_secrets(app.id())
        .await
        .expect("the store can stored secrets");
    assert!(!stored[0].metadata.unavailable);
    assert!(key.exists());
}

/// A key that still decrypts a stored value or webhook secret is not lost,
/// even when the verifier is corrupt: recovery refuses and discards nothing.
#[tokio::test]
async fn key_recovery_keeps_values_behind_a_corrupt_verifier() {
    for webhook in [false, true] {
        let (temp, store) = Store::temporary().await;
        let key = temp.path().join("secrets.key");
        let app = with_secret(&application());
        store
            .save_application(&ApplicationTemplate::from(&app), None, None)
            .await
            .expect("the store can save application");
        if webhook {
            store
                .generate_webhook_secret(Daemon, app.id())
                .await
                .expect("the store can generate webhook secret");
        } else {
            store
                .put_stored_secret(Daemon, app.id(), "token", 0, b"original".to_vec(), None)
                .await
                .expect("the store can put stored secret");
        }
        sqlx::query("UPDATE secret_key_verification SET ciphertext=zeroblob(length(ciphertext))")
            .execute(&store.pool)
            .await
            .expect("the statement executes");
        let Err(StoreError::SecretSource(error)) = store.recover_secret_key(Daemon).await else {
            panic!("recovery refused expected");
        };
        assert!(matches!(
            error.key_failure(),
            Some(crate::secrets::KeyFailure::VerifierCorrupt)
        ));
        assert!(key.exists());
        let kept = sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM webhook_secrets)+(SELECT COUNT(*) FROM application_secret_versions WHERE available=1)",
        )
        .fetch_one(&store.pool)
        .await
        .expect("the row is present");
        assert_eq!(kept, 1);
    }
}

/// Values need their key even without a verifier, so a missing key is
/// recovered then too.
#[tokio::test]
async fn key_recovery_needs_no_verifier() {
    let (temp, store, _, _) = stored_token().await;
    sqlx::query("DELETE FROM secret_key_verification")
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    std::fs::remove_file(temp.path().join("secrets.key")).expect("the file is removed");
    let recovery = store
        .recover_secret_key(Daemon)
        .await
        .expect("the store can recover secret key");
    assert_eq!(recovery.discarded_versions, 1);
}

/// Without a verifier, a write binds no key over values it can't decrypt,
/// neither a wrong one nor a new one for a missing key, but binds the
/// original again.
#[tokio::test]
async fn writes_bind_only_the_key_of_values_without_a_verifier() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, store, app, _) = stored_token().await;
    let key = temp.path().join("secrets.key");
    let original = Zeroizing::new(std::fs::read(&key).expect("the file reads"));
    sqlx::query("DELETE FROM secret_key_verification")
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    let write = async || {
        store
            .put_stored_secret(Daemon, app.id(), "other", 0, b"value".to_vec(), None)
            .await
    };
    std::fs::write(&key, [42; 32]).expect("the file writes");
    let Err(StoreError::SecretSource(error)) = write().await else {
        panic!("a wrong key must not be bound");
    };
    assert!(matches!(
        error.key_failure(),
        Some(crate::secrets::KeyFailure::AuthenticationFailed)
    ));
    std::fs::remove_file(&key).expect("the file is removed");
    assert!(matches!(write().await, Err(StoreError::SecretSource(_))));
    assert!(!key.exists(), "no key is generated over stored values");
    std::fs::write(&key, &*original).expect("the file writes");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
        .expect("the permissions apply");
    write().await.expect("the file writes");
    assert!(matches!(
        store.recover_secret_key(Daemon).await,
        Err(StoreError::SecretKeyUsable)
    ));
}

#[tokio::test]
async fn mounted_declared_secrets_are_generated_once_until_regenerated() {
    use piqueld_core::manifest::{SecretDeclaration, SecretEncoding, SecretGenerator, SecretMount};
    let (_temp, store) = Store::temporary().await;
    let mut manifest = with_secret(&application()).to_manifest();
    manifest.spec.services[0].secrets.push(SecretMount {
        name: "manual".into(),
        target: "/run/secrets/manual".into(),
    });
    for name in ["token", "manual", "unused"] {
        manifest.spec.secrets.push(SecretDeclaration {
            name: name.into(),
            generate: SecretGenerator::Random {
                bytes: 16,
                encoding: SecretEncoding::Hex,
            },
        });
    }
    let app = manifest
        .validate()
        .expect("the manifest validates")
        .normalize(application().id().clone());
    let first = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let env = store.production(app.id()).await;
    store
        .put_secret(Daemon, &env, "manual", 0, b"chosen".to_vec())
        .await
        .expect("the store can put secret");
    let pins = store
        .pin_secrets(&first, &app)
        .await
        .expect("the store can pin secrets");
    let value = store
        .secret_plaintext(&env, &pins["token"])
        .await
        .expect("the store can secret plaintext");
    assert_eq!(value.len(), 32);

    let second = store
        .save_application(&ApplicationTemplate::from(&app), None, Some(1))
        .await
        .expect("the store can save application");
    assert_eq!(
        store
            .pin_secrets(&second, &app)
            .await
            .expect("the store can pin secrets"),
        pins
    );
    let generations = store
        .secrets(&env)
        .await
        .expect("the store can secrets")
        .into_iter()
        .map(|secret| (secret.name, secret.generation))
        .collect::<Vec<_>>();
    assert_eq!(generations, [("manual".into(), 1), ("token".into(), 1)]);

    let token = store
        .regenerate_secret(Daemon, &env, "token", 1)
        .await
        .expect("the store can regenerate secret");
    assert_eq!(token.generation, 2);
    assert_eq!(
        store
            .pin_secrets(&second, &app)
            .await
            .expect("the store can pin secrets"),
        pins
    );
    let third = store
        .save_application(&ApplicationTemplate::from(&app), None, Some(2))
        .await
        .expect("the store can save application");
    let regenerated = store
        .pin_secrets(&third, &app)
        .await
        .expect("the store can pin secrets");
    let new_value = store
        .secret_plaintext(&env, &regenerated["token"])
        .await
        .expect("the store can secret plaintext");
    assert_ne!(&*new_value, &*value);
    assert!(matches!(
        store.regenerate_secret(Daemon, &env, "undeclared", 0).await,
        Err(StoreError::NotFound)
    ));
}

/// Key recovery leaves nothing of a generated value to keep, so the next
/// deployment that mounts it generates a new one. That is the only way back
/// when the manifest declaring it was never accepted, e.g. after a failed
/// repository deployment, since `regenerate_secret` reads the accepted one.
#[tokio::test]
async fn deployments_replace_generated_values_discarded_by_key_recovery() {
    let (temp, store) = Store::temporary().await;
    let accepted = ApplicationTemplate::from(&application());
    let first = store
        .save_application(&accepted, None, None)
        .await
        .expect("the store can save application");
    let (app, env) = (with_generated_secret(&application()), &first.environment_id);
    store
        .pin_secrets(&first, &app)
        .await
        .expect("the store can pin secrets");
    std::fs::write(temp.path().join("secrets.key"), [42; 32]).expect("the file writes");
    store
        .recover_secret_key(Daemon)
        .await
        .expect("the store can recover secret key");
    assert!(store.secrets(env).await.expect("the store can secrets")[0].unavailable);
    assert!(matches!(
        store.regenerate_secret(Daemon, env, "token", 1).await,
        Err(StoreError::NotFound)
    ));

    let second = store
        .save_application(&accepted, None, Some(1))
        .await
        .expect("the store can save application");
    let pins = store
        .pin_secrets(&second, &app)
        .await
        .expect("the store can pin secrets");
    let token = &store.secrets(env).await.expect("the store can secrets")[0];
    assert_eq!((token.generation, token.unavailable), (2, false));
    let value = store
        .secret_plaintext(env, &pins["token"])
        .await
        .expect("the store can secret plaintext");
    assert_eq!(value.len(), 32);
}

#[tokio::test]
async fn sibling_environment_deletions_do_not_block_another_environments_deployment() {
    use crate::{Mutation, MutationResponse};
    let (_temp, store) = Store::temporary().await;
    let app = application();
    let captured = with_generated_secret(&app);
    store
        .save_application(&ApplicationTemplate::from(&captured), None, None)
        .await
        .expect("the store can save application");
    let production = store.production(app.id()).await;
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: app.id().clone(),
                name: piqueld_core::EnvironmentName::parse("staging")
                    .expect("the environment name is valid"),
                source: None,
            },
            Some(1),
            false,
            None,
        )
        .await
        .expect("the store can accept")
    else {
        panic!("environment")
    };
    for environment in [&production, &staging.id] {
        store
            .put_secret(Daemon, environment, "token", 0, b"value".to_vec())
            .await
            .expect("the store can put secret");
    }
    let deployment = store
        .request_deploy(&production, Some(2))
        .await
        .expect("the store can request deploy");
    save(&store, &app, 2).await.expect("the store can save");
    store
        .begin_secret_deletion(Daemon, &staging.id, "token", 1)
        .await
        .expect("the store can begin secret deletion");
    assert_eq!(
        store
            .pin_secrets(&deployment, &captured)
            .await
            .expect("the store can pin secrets")
            .len(),
        1
    );
    assert!(matches!(
        save(&store, &captured, 3).await,
        Err(StoreError::SecretDeleting)
    ));
}

/// Seeds account `dev` with developer access and its session, and returns
/// the caller acting with it.
async fn developer(store: &Store) -> crate::Actor<'static> {
    use piqueld_core::access::{Preset, Scope};
    let grants = Preset::Developer.grants(&Scope::All);
    store.seed_caller("dev", &grants).await
}

/// Demotes `dev` (see `developer`) to read-only access, and returns the
/// refusal its secret writes now get.
async fn demote_developer(store: &Store) -> piqueld_core::access::Denied {
    use piqueld_core::access::{AppPermission, Denied, Permission, Preset, Scope};
    let mut db = store
        .pool
        .acquire()
        .await
        .expect("a connection is acquired");
    crate::auth::access::Holder::User("dev")
        .replace(&mut db, &Preset::ReadOnly.grants(&Scope::All))
        .await
        .expect("the store can replace");
    Denied::Missing(Permission::App(AppPermission::SecretsWrite))
}

/// Kinds of `app`'s events recorded as caused by `dev`.
async fn developer_events(
    store: &Store,
    app: &NormalizedApplication,
) -> std::collections::BTreeSet<String> {
    use crate::Visibility;
    let filter = piqueld_core::observability::EventFilter {
        application_id: Some(app.id().to_string()),
        ..Default::default()
    };
    store
        .filtered_events(&filter, &Visibility::ALL, None, 100)
        .await
        .expect("the store can filtered events")
        .items
        .into_iter()
        .filter(|event| {
            matches!(&event.actor, Some(piqueld_core::EventActor::Account { user_id, .. }) if user_id == "dev")
        })
        .map(|event| event.kind)
        .collect()
}

/// Secret writes check the caller's grants inside their transaction, so a
/// caller demoted after authenticating cannot store or delete values. Their
/// events, including the deletion's runtime action, record the caller.
#[tokio::test]
async fn secret_writes_use_the_callers_current_grants() {
    let (_temp, store) = Store::temporary().await;
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let production = store.production(app.id()).await;
    let caller = developer(&store).await;
    store
        .put_secret(caller, &production, "token", 0, b"value".to_vec())
        .await
        .expect("the store can put secret");
    let by = caller.attribution();
    let deletion = store
        .begin_secret_deletion(caller, &production, "token", 1)
        .await
        .expect("the store can begin secret deletion");
    store
        .begin_application_action(
            by,
            &production,
            piqueld_core::Phase::RemoveSecrets,
            Some("token"),
        )
        .await
        .expect("the store can begin application action");
    store
        .interrupt_actions(None)
        .await
        .expect("the store can interrupt actions");
    store
        .finish_secret_deletion(caller, &production, "token", &deletion.id)
        .await
        .expect("the store can finish secret deletion");
    let expected = [
        "secret_saved",
        "action_started",
        "action_outcome_unknown",
        "secret_deleted",
    ];
    assert_eq!(
        developer_events(&store, &app).await,
        expected.map(String::from).into()
    );
    store
        .put_secret(caller, &production, "token", 0, b"value".to_vec())
        .await
        .expect("the store can put secret");
    let refused = demote_developer(&store).await;
    assert!(matches!(
        store.put_secret(caller, &production, "token", 1, b"again".to_vec()).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.begin_secret_deletion(caller, &production, "token", 1).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
}

/// Writes to the application's store check the caller's grants like
/// environment secret writes: a demoted caller cannot store, delete, or
/// change who may mount a value. Their events record the caller.
#[tokio::test]
async fn stored_secret_writes_use_the_callers_current_grants() {
    let (_temp, store) = Store::temporary().await;
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    let caller = developer(&store).await;
    let access = piqueld_core::api::SecretAccess::default();
    store
        .put_stored_secret(caller, app.id(), "stripe", 0, b"value".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    store
        .set_secret_access(caller, app.id(), "stripe", &access, None)
        .await
        .expect("the store can set secret access");
    assert_eq!(
        developer_events(&store, &app).await,
        ["secret_access_changed", "secret_saved"]
            .map(String::from)
            .into()
    );
    let refused = demote_developer(&store).await;
    assert!(matches!(
        store.put_stored_secret(caller, app.id(), "stripe", 1, b"again".to_vec(), None).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.set_secret_access(caller, app.id(), "stripe", &access, None).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.begin_stored_secret_deletion(caller, app.id(), "stripe", 1).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
}

#[tokio::test]
async fn secret_key_check_reports_the_key_without_changing_it() {
    use piqueld_core::api::CheckStatus;
    let (temp, store) = Store::temporary().await;
    let key = temp.path().join("secrets.key");
    let status = async || store.secret_key_check().await.status;
    assert_eq!(status().await, CheckStatus::Pass);
    assert!(!key.exists());

    let app = with_secret(&application());
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the store can save application");
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"value".to_vec(), None)
        .await
        .expect("the store can put stored secret");
    assert_eq!(status().await, CheckStatus::Pass);
    let original = Zeroizing::new(std::fs::read(&key).expect("the file reads"));

    std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o640))
        .expect("the permissions apply");
    assert_eq!(status().await, CheckStatus::Fail);
    std::fs::write(&key, [42; 32]).expect("the file writes");
    std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("the permissions apply");
    let mismatched = store.secret_key_check().await;
    assert_eq!(mismatched.status, CheckStatus::Fail);
    assert!(
        mismatched
            .next_step
            .expect("the store can next step")
            .contains("recover-key")
    );
    std::fs::remove_file(&key).expect("the file is removed");
    assert_eq!(status().await, CheckStatus::Fail);
    assert!(
        !key.exists(),
        "a key bound to stored values is never regenerated"
    );

    sqlx::query("DELETE FROM secret_key_verification")
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    assert_eq!(status().await, CheckStatus::Fail, "missing key");
    std::fs::write(&key, [42; 32]).expect("the file writes");
    std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("the permissions apply");
    assert_eq!(status().await, CheckStatus::Fail, "wrong key");
    std::fs::write(&key, &*original).expect("the file writes");
    assert_eq!(status().await, CheckStatus::Pass, "original key");
    let verifiers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secret_key_verification")
        .fetch_one(&store.pool)
        .await
        .expect("the row is present");
    assert_eq!(verifiers, 0);
}

/// Each stage of a requested secret deletion commits with its own audit
/// record, while resuming a reservation and a stale completion change
/// nothing, record nothing, and leave their request's background record.
/// Key recovery records what it discarded.
#[tokio::test]
async fn requested_secret_deletions_and_recovery_record_each_committed_stage() {
    let temp = tempfile::tempdir().expect("a temporary directory is created");
    let store = Store::open_unsynced(temp.path().join("db"))
        .await
        .expect("the store opens");
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the application is saved");
    let env = store.production(app.id()).await;
    store
        .put_secret(Daemon, &env, "token", 0, b"value".to_vec())
        .await
        .expect("the generated secret is stored");
    store
        .put_stored_secret(Daemon, app.id(), "stripe", 0, b"value".to_vec(), None)
        .await
        .expect("the stored secret to delete is stored");
    store
        .put_stored_secret(Daemon, app.id(), "kept", 0, b"value".to_vec(), None)
        .await
        .expect("the stored secret to discard is stored");
    let route = "DELETE /api/v1/environments/{id}/secrets/{name}";
    macro_rules! stages {
        ($begin:ident, $finish:ident, $owner:expr, $name:literal) => {{
            let requests = [(); 4].map(|()| crate::testing::audit_request(route));
            let by = |stage: usize| crate::Author {
                actor: Daemon,
                request: Some(&requests[stage]),
            };
            let deletion = store
                .$begin(by(0), $owner, $name, 1)
                .await
                .expect("the deletion is reserved");
            store
                .$begin(by(1), $owner, $name, 1)
                .await
                .expect("the reservation resumes");
            for stage in [2, 3] {
                store
                    .$finish(by(stage), $owner, $name, &deletion.id)
                    .await
                    .expect("completing the deletion succeeds, or does nothing");
            }
            let recorded = requests.each_ref().map(|request| request.recorded.get());
            assert_eq!(recorded, [true, false, true, false]);
            deletion.id
        }};
    }
    let token = stages!(begin_secret_deletion, finish_secret_deletion, &env, "token");
    let stripe = stages!(
        begin_stored_secret_deletion,
        finish_stored_secret_deletion,
        app.id(),
        "stripe"
    );
    std::fs::write(temp.path().join("secrets.key"), [42; 32]).expect("the lost key is written");
    let request = crate::testing::audit_request(route);
    let author = crate::Author {
        actor: Daemon,
        request: Some(&request),
    };
    store
        .recover_secret_key(author)
        .await
        .expect("the lost key is recovered");
    assert!(request.recorded.get());
    assert_eq!(
        store.audit_actions().await,
        [
            format!("{route} secret \"token\" deletion {token} reserved"),
            format!("{route} secret \"token\" deletion {token} completed"),
            format!("{route} secret \"stripe\" deletion {stripe} reserved"),
            format!("{route} secret \"stripe\" deletion {stripe} completed"),
            format!(
                "{route} secret key recovery discarded 1 values across 0 environments \
                and 1 applications, 0 webhook secrets, and 0 notification destinations"
            ),
        ]
    );
}

/// Key recovery records the webhook secrets it deletes, even when they are
/// all it discards.
#[tokio::test]
async fn key_recovery_records_the_webhook_secrets_it_deletes() {
    let temp = tempfile::tempdir().expect("a temporary directory is created");
    let store = Store::open_unsynced(temp.path().join("db"))
        .await
        .expect("the store opens");
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .expect("the application is saved");
    store
        .generate_webhook_secret(Daemon, app.id())
        .await
        .expect("the webhook secret is generated");
    std::fs::write(temp.path().join("secrets.key"), [42; 32]).expect("the lost key is written");
    let route = "POST /api/v1/system/secrets/recover-key";
    let request = crate::testing::audit_request(route);
    let author = crate::Author {
        actor: Daemon,
        request: Some(&request),
    };
    store
        .recover_secret_key(author)
        .await
        .expect("the lost key is recovered");
    assert_eq!(
        store.audit_actions().await,
        [format!(
            "{route} secret key recovery discarded 0 values across 0 environments \
            and 0 applications, 1 webhook secrets, and 0 notification destinations"
        )]
    );
}
