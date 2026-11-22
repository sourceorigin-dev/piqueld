use super::*;
use crate::Actor::Daemon;
use crate::{Mutation, MutationResponse};
use piqueld_core::{
    EnvironmentName,
    manifest::{ApplicationTemplate, SecretMount, Variable},
};

/// An application whose `web` service mounts the stored secret its
/// environment's `stripe_key` names (`stripe-dev` unless overridden), and the
/// generated secret `session`.
fn template() -> ApplicationTemplate {
    let mut manifest = crate::testing::prebuilt("app-stored-test").to_manifest();
    manifest
        .spec
        .variables
        .insert("stripe_key".into(), Variable::String("stripe-dev".into()));
    manifest.spec.environments.insert(
        "production".into(),
        piqueld_core::manifest::EnvironmentConfig {
            variables: [(
                "stripe_key".to_owned(),
                Variable::String("stripe-prod".into()),
            )]
            .into(),
            ..Default::default()
        },
    );
    manifest.spec.services[0].secrets = vec![
        SecretMount {
            name: "${{ vars.stripe_key }}".into(),
            target: "/run/secrets/stripe".into(),
        },
        SecretMount {
            name: "session".into(),
            target: "/run/secrets/session".into(),
        },
    ];
    manifest.spec.secrets = serde_json::from_value(serde_json::json!([
        {"name":"session", "generate":{"type":"random", "bytes":16}}
    ]))
    .unwrap();
    manifest
        .validate_template()
        .unwrap()
        .normalize(ApplicationId::parse("app-stored-test").unwrap())
}

/// Access limited to `environments`, without previews.
fn only<'a>(environments: impl IntoIterator<Item = &'a EnvironmentId>) -> SecretAccess {
    SecretAccess {
        environments: EnvironmentAccess::Only(environments.into_iter().cloned().collect()),
        previews: false,
    }
}

/// Saves `template()` with a `staging` environment next to `production`.
/// Returns the store's directory, the store, and both environment IDs.
async fn fixture() -> (tempfile::TempDir, Store, EnvironmentId, EnvironmentId) {
    let (temp, store) = Store::temporary().await;
    let template = template();
    store.save_application(&template, None, None).await.unwrap();
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: template.id().clone(),
                name: EnvironmentName::parse("staging").unwrap(),
                source: None,
            },
            Some(1),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("environment")
    };
    let production = store.production(template.id()).await;
    (temp, store, production, staging.id)
}

/// Captures a deployment of `environment` and pins its secrets.
async fn deploy(
    store: &Store,
    environment: &EnvironmentId,
) -> Result<BTreeMap<String, String>, StoreError> {
    let operation = store.request_deploy(environment, None).await?;
    let rendered = store
        .deployment_snapshot(&operation.id)
        .await?
        .rendering
        .unwrap()
        .application;
    store.pin_secrets(&operation, &rendered).await
}

#[tokio::test]
async fn environments_mount_the_stored_secret_their_variables_name_where_access_allows() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-prod",
            0,
            b"live".to_vec(),
            Some(&only([&production])),
        )
        .await
        .unwrap();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            None,
        )
        .await
        .unwrap();

    // Each environment mounts its own stored secret through `stripe_key`, and
    // its own generated `session`.
    let production_pins = deploy(&store, &production).await.unwrap();
    let staging_pins = deploy(&store, &staging).await.unwrap();
    for (environment, pins, value) in [
        (&production, &production_pins, &b"live"[..]),
        (&staging, &staging_pins, &b"test"[..]),
    ] {
        let stripe = pins
            .iter()
            .find(|(name, _)| name.starts_with("stripe"))
            .unwrap()
            .1;
        assert_eq!(
            &*store.secret_plaintext(environment, stripe).await.unwrap(),
            value
        );
        assert_eq!(store.secrets(environment).await.unwrap()[0].name, "session");
    }
    assert_eq!(
        production_pins.keys().collect::<Vec<_>>(),
        ["session", "stripe-prod"]
    );
    assert_eq!(
        staging_pins.keys().collect::<Vec<_>>(),
        ["session", "stripe-dev"]
    );

    // A secret every environment may mount gets one Docker secret per environment.
    let mut manifest = template().to_manifest();
    manifest.spec.environments.clear();
    let shared = manifest
        .validate_template()
        .unwrap()
        .normalize(application.clone());
    store
        .force(Mutation::Save {
            application: Box::new(shared),
            expected_application_id: Some(application.to_string()),
            deploy: false,
        })
        .await
        .unwrap();
    let production_copy = deploy(&store, &production).await.unwrap()["stripe-dev"].clone();
    assert_ne!(production_copy, staging_pins["stripe-dev"]);
    assert_eq!(
        &*store
            .secret_plaintext(&production, &production_copy)
            .await
            .unwrap(),
        b"test"
    );
    assert!(
        store
            .secret_plaintext(&production, &staging_pins["stripe-dev"])
            .await
            .is_err(),
        "an environment cannot read another environment's Docker secret"
    );
}

#[tokio::test]
async fn capture_and_pinning_deny_stored_secrets_outside_the_access_list() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            None,
        )
        .await
        .unwrap();
    // Captured while staging may still mount it, pinned after access narrowed.
    let captured = store.request_deploy(&staging, None).await.unwrap();
    store
        .set_secret_access(
            Daemon,
            &application,
            "stripe-dev",
            &only([&production]),
            None,
        )
        .await
        .unwrap();
    let denied = |result: Result<_, StoreError>| {
        matches!(
            result,
            Err(StoreError::SecretAccessDenied { environment, secret })
                if environment.as_str() == "staging" && secret == "stripe-dev"
        )
    };
    assert!(denied(
        store.request_deploy(&staging, None).await.map(|_| ())
    ));
    let rendered = store
        .deployment_snapshot(&captured.id)
        .await
        .unwrap()
        .rendering
        .unwrap()
        .application;
    assert!(denied(
        store.pin_secrets(&captured, &rendered).await.map(|_| ())
    ));
    let environment = store.get(&staging).await.unwrap().environment;
    assert!(denied(
        store.check_secret_access(&environment, &rendered).await
    ));
}

/// An access change made from the access it read fails once someone else
/// changed it, keeping theirs; made from the current one, it applies.
#[tokio::test]
async fn access_changes_from_an_outdated_read_are_refused() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    let secret = "stripe-dev";
    store
        .put_stored_secret(Daemon, &application, secret, 0, b"test".to_vec(), None)
        .await
        .unwrap();
    let read = SecretAccess::default();
    let theirs = only([&production]);
    store
        .set_secret_access(Daemon, &application, secret, &theirs, Some(&read))
        .await
        .unwrap();
    let mine = only([&staging]);
    assert!(matches!(
        store
            .set_secret_access(Daemon, &application, secret, &mine, Some(&read))
            .await,
        Err(StoreError::SecretAccessConflict)
    ));
    let current = store
        .set_secret_access(Daemon, &application, secret, &mine, Some(&theirs))
        .await
        .unwrap();
    assert_eq!(current.access, mine);
}

/// A value upload that replaces the access, made from the access it read,
/// fails once someone else changed it, keeping theirs.
#[tokio::test]
async fn uploads_from_an_outdated_access_read_are_refused() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    let secret = "stripe-dev";
    let read = store
        .put_stored_secret(Daemon, &application, secret, 0, b"one".to_vec(), None)
        .await
        .unwrap()
        .access;
    let theirs = only([&production]);
    store
        .set_secret_access(Daemon, &application, secret, &theirs, Some(&read))
        .await
        .unwrap();
    let mine = only([&staging]);
    let upload = |expected| {
        store.put_stored_secret(
            Daemon,
            &application,
            secret,
            SecretPrecondition {
                generation: 1,
                access: expected,
            },
            b"two".to_vec(),
            Some(&mine),
        )
    };
    assert!(matches!(
        upload(Some(&read)).await,
        Err(StoreError::SecretAccessConflict)
    ));
    assert_eq!(upload(Some(&theirs)).await.unwrap().access, mine);
}

#[tokio::test]
async fn a_name_is_either_declared_and_generated_or_stored() {
    let (_temp, store, _, _) = fixture().await;
    let application = template().id().clone();
    let conflict = |result: Result<(), StoreError>| match result {
        Err(StoreError::Validation(errors)) => {
            errors.0.len() == 1
                && errors.0[0].code == ApiErrorCode::SecretNameConflict
                && errors.0[0].path == "spec.secrets[0].name"
        }
        _ => false,
    };
    assert!(conflict(
        store
            .put_stored_secret(Daemon, &application, "session", 0, b"manual".to_vec(), None)
            .await
            .map(|_| ())
    ));
    store
        .put_stored_secret(Daemon, &application, "api-key", 0, b"manual".to_vec(), None)
        .await
        .unwrap();
    let mut manifest = template().to_manifest();
    manifest.spec.secrets[0].name = "api-key".into();
    manifest.spec.services[0].secrets[1].name = "api-key".into();
    let edit = piqueld_core::edit::ApplicationEdit::GeneratedSecrets(manifest.spec.secrets.clone());
    let declared = manifest
        .validate_template()
        .unwrap()
        .normalize(application.clone());
    assert!(conflict(
        store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(declared),
                    expected_application_id: Some(application.to_string()),
                    deploy: false,
                },
                None,
                true,
                None,
            )
            .await
            .map(|_| ())
    ));
    // Editing only the declarations is checked the same way.
    assert!(conflict(
        store
            .accept(
                Daemon,
                Mutation::Edit {
                    id: application.clone(),
                    edit: Box::new(edit),
                    deploy: false,
                },
                None,
                true,
                None,
            )
            .await
            .map(|_| ())
    ));
}

#[tokio::test]
async fn access_lists_keep_renamed_environments_and_drop_deleted_ones() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    let stored = store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            Some(&only([&staging])),
        )
        .await
        .unwrap();
    assert_eq!(stored.access, only([&staging]));

    // A renamed environment keeps its access and is shown by its new name.
    store
        .force(Mutation::RenameEnvironment {
            id: staging.clone(),
            name: EnvironmentName::parse("qa").unwrap(),
        })
        .await
        .unwrap();
    let access = store.stored_secrets(&application).await.unwrap()[0]
        .access
        .clone();
    assert_eq!(access, only([&staging]));
    let environments = store.environments(&application).await.unwrap();
    assert_eq!(access.describe(&environments), "qa");
    deploy(&store, &staging).await.unwrap();

    // Deleting an environment removes it from every list. Every-environment
    // access is unaffected, and covers environments created later.
    store
        .put_stored_secret(Daemon, &application, "shared", 0, b"all".to_vec(), None)
        .await
        .unwrap();
    let deletion = store.request_delete(&staging).await.unwrap();
    sqlx::query!(
        "UPDATE operations SET state='running' WHERE id=?1",
        deletion.id
    )
    .execute(&store.pool)
    .await
    .unwrap();
    store.finish_delete_operation(&deletion).await.unwrap();
    let secrets = store.stored_secrets(&application).await.unwrap();
    assert_eq!(secrets[0].metadata.name, "shared");
    assert_eq!(secrets[0].access, SecretAccess::default());
    assert_eq!(secrets[1].access, only([]));
    assert_eq!(secrets[1].access.describe(&[]), "no environment");
    let production_view = store.get(&production).await.unwrap().environment;
    assert!(!secrets[1].access.allows(&production_view));
    assert!(matches!(
        store
            .set_secret_access(Daemon, &application, "shared", &only([&staging]), None)
            .await,
        Err(StoreError::InvalidInput)
    ));
}

/// The 101st secret of a store exceeds its quota, like too many versions.
#[tokio::test]
async fn a_store_holds_at_most_100_secrets() {
    let (_temp, store, _, _) = fixture().await;
    let application = template().id().clone();
    let put = |name: String| {
        let (store, application) = (&store, &application);
        async move {
            store
                .put_stored_secret(Daemon, application, &name, 0, b"value".to_vec(), None)
                .await
        }
    };
    for index in 0..100 {
        put(format!("secret-{index}")).await.unwrap();
    }
    assert!(matches!(
        put("secret-100".into()).await,
        Err(StoreError::SecretQuota)
    ));
}

/// A secret being deleted keeps the access it had, like its value.
#[tokio::test]
async fn access_is_fixed_while_a_secret_is_deleted() {
    let (_temp, store, _, staging) = fixture().await;
    let application = template().id().clone();
    store
        .put_stored_secret(Daemon, &application, "shared", 0, b"all".to_vec(), None)
        .await
        .unwrap();
    store
        .begin_stored_secret_deletion(Daemon, &application, "shared", 1)
        .await
        .unwrap();
    assert!(matches!(
        store
            .set_secret_access(Daemon, &application, "shared", &only([&staging]), None)
            .await,
        Err(StoreError::SecretDeleting)
    ));
    let secret = &store.stored_secrets(&application).await.unwrap()[0];
    assert_eq!(secret.access, SecretAccess::default());
}

/// The generations of the versions of `name` in `table`, which holds
/// generated or stored versions or copies.
async fn generations(store: &Store, table: &str, name: &str) -> Vec<i64> {
    sqlx::query_scalar(&format!(
        "SELECT generation FROM {table} WHERE name=?1 ORDER BY generation"
    ))
    .bind(name)
    .fetch_all(&store.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn versions_go_once_no_deployment_pins_them_and_they_are_not_current() {
    let (_temp, store, production, _) = fixture().await;
    let (application, access) = (template().id().clone(), only([&production]));
    let put = |expected| {
        store.put_stored_secret(
            Daemon,
            &application,
            "stripe-prod",
            expected,
            b"live".to_vec(),
            (expected == 0).then_some(&access),
        )
    };
    put(0).await.unwrap();
    let first = deploy(&store, &production).await.unwrap();
    let pinning = store.deployments(&production, None, 1).await.unwrap().items[0]
        .operation
        .id
        .clone();
    put(1).await.unwrap();
    store
        .regenerate_secret(Daemon, &production, "session", 1)
        .await
        .unwrap();
    deploy(&store, &production).await.unwrap();
    // The third generated version is current, but no deployment pins it.
    store
        .regenerate_secret(Daemon, &production, "session", 2)
        .await
        .unwrap();
    // The third stored version is never deployed, and the fourth is current.
    put(2).await.unwrap();
    put(3).await.unwrap();

    // The first deployment still pins the first versions.
    assert!(store.unused_secret_versions().await.unwrap().is_empty());
    // As if deployment history pruned it.
    sqlx::query("DELETE FROM operations WHERE id=?1")
        .bind(&pinning)
        .execute(&store.pool)
        .await
        .unwrap();
    // A running target still naming the first generated version keeps it.
    let resolved = |names: serde_json::Value| {
        sqlx::query("UPDATE environments SET resolved_json=?1 WHERE id=?2")
            .bind(
                names
                    .is_object()
                    .then(|| serde_json::json!({"secret_names": names}).to_string()),
            )
            .bind(production.as_str())
            .execute(&store.pool)
    };
    resolved(serde_json::json!({"session": first["session"]}))
        .await
        .unwrap();
    assert_eq!(
        store.unused_secret_versions().await.unwrap(),
        BTreeMap::from([(production.clone(), vec![first["stripe-prod"].clone()])])
    );
    resolved(serde_json::Value::Null).await.unwrap();
    let unused = store.unused_secret_versions().await.unwrap();
    let mut expected = vec![first["session"].clone(), first["stripe-prod"].clone()];
    expected.sort();
    assert_eq!(
        unused,
        BTreeMap::from([(production.clone(), expected.clone())])
    );

    store
        .forget_secret_versions(&production, &expected)
        .await
        .unwrap();
    store.forget_uncopied_secret_versions().await.unwrap();
    assert!(store.unused_secret_versions().await.unwrap().is_empty());
    assert_eq!(
        generations(&store, "secret_versions", "session").await,
        [2, 3]
    );
    assert_eq!(
        generations(&store, "application_secret_copies", "stripe-prod").await,
        [2]
    );
    assert_eq!(
        generations(&store, "application_secret_versions", "stripe-prod").await,
        [2, 4]
    );
}

/// A requested secret change commits with its audit record, naming the
/// secret, its generation and access but never its value: a failed append
/// keeps the previous version and access, and stores no new version.
#[tokio::test]
async fn requested_secret_changes_commit_with_their_audit_record() {
    let (_temp, store, _, staging) = fixture().await;
    let application = template().id().clone();
    let route = "PUT /api/v1/applications/{id}/secrets/{name}";
    let request = crate::testing::audit_request(route);
    let author = Author {
        actor: Daemon,
        request: Some(&request),
    };
    let staging_only = only([&staging]);
    let put = async |expected, access| {
        let value = b"hunter2".to_vec();
        store
            .put_stored_secret(author, &application, "stripe-dev", expected, value, access)
            .await
    };
    put(0, None).await.expect("the first upload is stored");
    store.refuse_audit(true).await;
    put(1, Some(&staging_only))
        .await
        .expect_err("`put` should be rejected");
    let access = store.set_secret_access(author, &application, "stripe-dev", &staging_only, None);
    access.await.expect_err("`access` should be rejected");
    store.refuse_audit(false).await;
    let [kept] = store
        .stored_secrets(&application)
        .await
        .expect("stored secrets are listed")
        .try_into()
        .expect("the store holds exactly one secret");
    assert_eq!(kept.metadata.generation, 1);
    assert_eq!(kept.access, SecretAccess::default());
    put(1, Some(&staging_only))
        .await
        .expect("the upload with access is stored");
    // Generated secrets, as regenerating stores them, too.
    let value = b"hunter2".to_vec();
    let generated = store.put_secret(author, &staging, "session", 0, value);
    generated.await.expect("the generated value is stored");
    // A regeneration whose record can't be written keeps the last version.
    store.refuse_audit(true).await;
    let regenerated = store.regenerate_secret(author, &staging, "session", 1);
    regenerated
        .await
        .expect_err("`regenerated` should be rejected");
    store.refuse_audit(false).await;
    let [session] = store
        .secrets(&staging)
        .await
        .expect("generated secrets are listed")
        .try_into()
        .expect("the environment holds exactly one generated secret");
    assert_eq!((session.name.as_str(), session.generation), ("session", 1));
    let versions = "SELECT COUNT(*) FROM secret_versions WHERE name='session'";
    let versions: i64 = sqlx::query_scalar(versions)
        .fetch_one(&store.pool)
        .await
        .expect("secret versions are counted");
    assert_eq!(versions, 1);
    assert_eq!(
        store.audit_actions().await,
        [
            format!("{route} secret \"stripe-dev\" generation 1"),
            format!("{route} secret \"stripe-dev\" generation 2 access staging"),
            format!("{route} secret \"session\" generation 1"),
        ]
    );
}
