use super::super::testing::template;
use super::*;

/// Saves a manifest declaring `staging` and `preview`, then creates the
/// environment `qa`.
async fn configured_environments(store: &Store) -> (EnvironmentId, EnvironmentId) {
    let (_, [staging, qa]) = environments(
        store,
        "[spec.environments.staging.variables]\nlevel='debug'\n[spec.environments.preview.variables]\nlevel='trace'",
        ["staging", "qa"],
    )
    .await;
    (staging, qa)
}

/// Saves the application `notes` with `spec`, then creates the
/// environments `names` it doesn't declare.
async fn environments<const N: usize>(
    store: &Store,
    spec: &str,
    names: [&str; N],
) -> (ApplicationId, [EnvironmentId; N]) {
    let saved = store.save_template(template("notes", spec), false).await;
    let application =
        ApplicationId::parse(saved.application_id).expect("the application id parses");
    let declared = store
        .environments(&application)
        .await
        .expect("environments succeeds");
    let mut ids = Vec::new();
    for name in names {
        if let Some(environment) = declared
            .iter()
            .find(|environment| environment.name.as_str() == name)
        {
            ids.push(environment.id.clone());
            continue;
        }
        let create = Mutation::CreateEnvironment {
            application: application.clone(),
            name: EnvironmentName::parse(name).expect("the environment name parses"),
            source: None,
        };
        let MutationResponse::Environment(environment) =
            store.force(create).await.expect("force succeeds")
        else {
            panic!("environment response")
        };
        ids.push(environment.id);
    }
    (
        application,
        ids.try_into().expect("the ids convert to an array"),
    )
}

fn rename(id: &EnvironmentId, name: &str) -> Mutation {
    Mutation::RenameEnvironment {
        id: id.clone(),
        name: EnvironmentName::parse(name).expect("the environment name parses"),
    }
}

/// Names select `[spec.environments.<name>]`, so a rename that would change
/// the applied block is refused.
#[tokio::test]
async fn renames_never_switch_the_configuration_an_environment_deploys() {
    let (_directory, store) = Store::temporary().await;
    let (staging, qa) = configured_environments(&store).await;
    for (id, name, configured) in [
        (&staging, "testing", "staging"),
        (&qa, "preview", "preview"),
    ] {
        assert!(matches!(
            store.force(rename(id, name)).await,
            Err(StoreError::EnvironmentConfigured { environment })
                if environment.as_str() == configured
        ));
    }
    store
        .force(rename(&qa, "testing"))
        .await
        .expect("force succeeds");
}

/// Renames keep a resolved environment resolved unless its configuration
/// renders the changed name, so it reports the redeploy it needs.
#[tokio::test]
async fn renames_unresolve_environments_that_render_the_name() {
    for (value, keeps_environment_rename, keeps_application_rename) in [
        ("${{ vars.level }}", true, true),
        ("${{ env.name }}", false, true),
        ("${{ app.name }}", true, false),
    ] {
        let (_directory, store) = Store::temporary().await;
        let (application, [id]) = environments(
            &store,
            &format!("[spec.variables]\nlevel='info'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'\n[spec.services.environment]\nVALUE='{value}'"),
            ["staging"],
        )
        .await;
        let resolved_after = async |mutation| {
            sqlx::query(
                "UPDATE environments SET resolved_generation=(SELECT generation FROM applications)",
            )
            .execute(&store.pool)
            .await
            .expect("execute succeeds");
            store
                .accept(Actor::Daemon, mutation, None, false, None)
                .await
                .expect("accept succeeds");
            let current = store.get(&id).await.expect("the environment exists");
            current.environment.resolved_generation == Some(current.application.generation)
        };
        assert_eq!(
            resolved_after(rename(&id, "qa")).await,
            keeps_environment_rename,
            "{value}"
        );
        assert_eq!(
            resolved_after(crate::testing::rename_application(&application, "journal")).await,
            keeps_application_rename,
            "{value}"
        );
    }
}

/// Lifecycle changes advance the application revision, so a second change
/// based on the same inspection fails instead of acting on a renamed environment.
#[tokio::test]
async fn environment_changes_from_a_stale_inspection_conflict() {
    let (_directory, store) = Store::temporary().await;
    let (_, qa) = configured_environments(&store).await;
    let inspected = store
        .get(&qa)
        .await
        .expect("the environment exists")
        .application
        .generation;
    store
        .accept(
            Actor::Daemon,
            rename(&qa, "testing"),
            Some(inspected),
            false,
            None,
        )
        .await
        .expect("accept succeeds");
    for stale in [rename(&qa, "review"), Mutation::Delete { id: qa.clone() }] {
        assert!(matches!(
            store.accept(Actor::Daemon, stale, Some(inspected), false, None).await,
            Err(StoreError::GenerationConflict { expected, actual })
                if expected == inspected && actual == inspected + 1
        ));
    }
    assert_eq!(
        store
            .get(&qa)
            .await
            .expect("the environment exists")
            .environment
            .name
            .as_str(),
        "testing"
    );
}

/// A manifest that declares environments creates exactly those, owns
/// their sources, and deploys the ones that build; one it no longer
/// declares keeps running until deleted explicitly.
#[tokio::test]
async fn declared_environments_are_managed_by_the_manifest() {
    use piqueld_core::EnvironmentSource;
    let (_directory, store) = Store::temporary().await;
    let service = "[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'\n";
    let save = async |spec: &str, deploy: bool| {
        let template = template("notes", &format!("{service}{spec}"));
        store.save_template_forced(template, deploy).await
    };
    let saved = save(
        "[spec.environments.staging]\n[spec.environments.production]\npromote_from='staging'\n",
        true,
    )
    .await;
    let application =
        ApplicationId::parse(&saved.application_id).expect("the application id parses");
    let named = async |name: &str| {
        store
            .environments(&application)
            .await
            .expect("environments succeeds")
            .into_iter()
            .find(|environment| environment.name.as_str() == name)
    };
    let (staging, production) = (
        named("staging").await.expect("the environment exists"),
        named("production").await.expect("the environment exists"),
    );
    assert_eq!(staging.source, EnvironmentSource::Saved);
    assert_eq!(production.source.promoted_from(), Some(&staging.id));
    let [deployment] = saved.operation_ids.as_slice() else {
        panic!("one deployment expected")
    };
    assert_eq!(
        store
            .operation(deployment)
            .await
            .expect("operation succeeds")
            .environment_id,
        staging.id
    );

    for refused in [
        Mutation::CreateEnvironment {
            application: application.clone(),
            name: staging.name.clone(),
            source: None,
        },
        Mutation::Delete {
            id: production.id.clone(),
        },
        Mutation::Promotion(PromotionMutation::SetSource {
            id: production.id.clone(),
            promote_from: None,
        }),
    ] {
        assert!(matches!(
            store.force(refused).await,
            Err(StoreError::EnvironmentDeclared { .. })
        ));
    }

    save(
        "[spec.environments.production]\n[spec.environments.qa.variables]\nlevel='debug'\n",
        false,
    )
    .await;
    assert_eq!(
        named("production")
            .await
            .expect("the environment exists")
            .source,
        EnvironmentSource::Saved
    );
    assert!(named("qa").await.is_some());
    store
        .force(Mutation::Delete { id: staging.id })
        .await
        .expect("force succeeds");
}

/// Blocks that set no source, as every block did before blocks declared
/// sources, keep the source an environment was given with `env source`
/// across saves, and still allow changing it that way.
#[tokio::test]
async fn blocks_without_a_source_keep_existing_sources() {
    let (_directory, store) = Store::temporary().await;
    let spec = "[spec.environments.staging.variables]\nlevel='debug'\n[spec.environments.production.variables]\nlevel='info'\n";
    let (application, [staging, production]) =
        environments(&store, spec, ["staging", "production"]).await;
    let promote = Mutation::Promotion(PromotionMutation::SetSource {
        id: production.clone(),
        promote_from: Some(staging.clone()),
    });
    store.force(promote).await.expect("force succeeds");
    let save = Mutation::save(
        template("notes", spec),
        Some(application.to_string()),
        false,
    );
    store.force(save).await.expect("force succeeds");
    let source = store
        .get(&production)
        .await
        .expect("the environment exists")
        .environment
        .source;
    assert_eq!(source.promoted_from(), Some(&staging));
}

/// Edits configure existing environments: one naming an environment that
/// doesn't exist, such as a typo, is refused instead of declaring an
/// environment no edit could remove, or of configuring nothing when it
/// would add no block.
#[tokio::test]
async fn edits_declare_no_new_environments() {
    use piqueld_core::manifest::Visibility;
    let (_directory, store) = Store::temporary().await;
    let (application, _) = environments(&store, "[spec.environments.staging]\n", ["staging"]).await;
    for visibility in [Visibility::Private, Visibility::Public] {
        let typo = Mutation::Edit {
            id: application.clone(),
            edit: Box::new(piqueld_core::edit::ApplicationEdit::EnvironmentVisibility {
                environment: "stagign".into(),
                visibility,
            }),
            deploy: false,
        };
        assert!(matches!(
            store.force(typo).await,
            Err(StoreError::EnvironmentUnknown { environment }) if environment == "stagign"
        ));
    }
    assert_eq!(
        store
            .environments(&application)
            .await
            .expect("environments succeeds")
            .len(),
        1
    );
}

/// `apply --deploy` never builds an environment its manifest makes
/// promoted, even while that change waits for a deployment in progress.
#[tokio::test]
async fn deploying_a_save_skips_environments_becoming_promoted() {
    let (_directory, store) = Store::temporary().await;
    let service = "[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'\n[spec.environments.staging]\n";
    let save = async |production: &str| {
        let spec = format!("{service}[spec.environments.production]\n{production}");
        store
            .save_template_forced(template("notes", &spec), true)
            .await
    };
    assert_eq!(save("").await.operation_ids.len(), 2);
    let saved = save("promote_from='staging'\n").await;
    let application =
        ApplicationId::parse(&saved.application_id).expect("the application id parses");
    let production = store.production(&application).await;
    assert!(
        store
            .get(&production)
            .await
            .expect("the environment exists")
            .environment
            .source
            .promoted_from()
            .is_none()
    );
    let [deployment] = saved.operation_ids.as_slice() else {
        panic!("only staging deploys")
    };
    assert_ne!(
        store
            .operation(deployment)
            .await
            .expect("operation succeeds")
            .environment_id,
        production
    );
}

/// Receipts recorded before deploy overrides existed must still replay.
#[test]
fn deploys_without_a_revision_keep_their_original_fingerprint() {
    let deploy =
        Mutation::deploy(EnvironmentId::parse("input-app").expect("the environment id parses"));
    let original = Sha256::digest(br#"[{"kind":"deploy","id":"input-app"},null,false]"#);
    assert_eq!(
        Store::mutation_fingerprint(&deploy, false).expect("mutation fingerprint succeeds"),
        format!("{original:x}")
    );
}

/// A change an API request makes commits with its audit record: a
/// failed append rolls back the change and its receipt, and a replay
/// records nothing new.
#[tokio::test]
async fn requested_changes_commit_with_their_audit_record() {
    let (_directory, store) = Store::temporary().await;
    let request = || crate::testing::audit_request("POST /api/v1/applications");
    let accept = async |request: &crate::AuditRequest| {
        let author = Author {
            actor: Actor::Daemon,
            request: Some(request),
        };
        let save = Mutation::save(template("shop", "[spec]"), None, false);
        store
            .accept(author, save, Some(0), false, Some("create-shop"))
            .await
    };
    let counts = async || {
        let mut counts = Vec::new();
        for table in ["applications", "request_receipts", "events", "audit_events"] {
            let count = format!("SELECT COUNT(*) FROM {table}");
            let count: i64 = sqlx::query_scalar(&count)
                .fetch_one(&store.pool)
                .await
                .expect("fetch one succeeds");
            counts.push(count);
        }
        counts
    };
    let before = counts().await;
    store.refuse_audit(true).await;
    let failed = request();
    accept(&failed)
        .await
        .expect_err("`accept` should be rejected");
    assert!(!failed.recorded.get());
    store.refuse_audit(false).await;
    assert_eq!(counts().await, before);
    let committed = request();
    let Ok((MutationResponse::Saved(saved), _)) = accept(&committed).await else {
        panic!("saved response expected")
    };
    assert!(committed.recorded.get());
    let replayed = request();
    accept(&replayed).await.expect("accept succeeds");
    assert!(!replayed.recorded.get());
    let rows = "SELECT action,outcome,status,user_id,application_id FROM audit_events";
    let rows = sqlx::query_as::<_, (String, String, i64, String, String)>(rows)
        .fetch_all(&store.pool)
        .await
        .expect("fetch all succeeds");
    let action = format!("POST /api/v1/applications generation {}", saved.generation);
    let allowed = (
        action,
        "allowed".into(),
        200,
        "alice".into(),
        saved.application_id,
    );
    assert_eq!(rows, [allowed]);
}

/// An environment change that finds its value already set, such as its
/// current name or source, changes nothing, so it records nothing and
/// succeeds even when audit records can't be written.
#[tokio::test]
async fn unchanged_environment_requests_record_nothing() {
    let (_directory, store) = Store::temporary().await;
    let (_, qa) = configured_environments(&store).await;
    let source = Mutation::Promotion(PromotionMutation::SetSource {
        id: qa.clone(),
        promote_from: None,
    });
    store.refuse_audit(true).await;
    for unchanged in [rename(&qa, "qa"), source] {
        let request = crate::testing::audit_request("PUT /api/v1/environments/{id}");
        let author = Author {
            actor: Actor::Daemon,
            request: Some(&request),
        };
        let accepted = store.accept(author, unchanged, None, false, None).await;
        assert!(accepted.is_ok() && !request.recorded.get());
    }
    store.refuse_audit(false).await;
    assert_eq!(store.audit_actions().await, Vec::<String>::new());
}

/// Renaming an application to its own name changes nothing: no record,
/// even when audit records can't be written, no `application_renamed`
/// event, and the same revision and update time.
#[tokio::test]
async fn renaming_an_application_to_its_name_records_nothing() {
    let (_directory, store) = Store::temporary().await;
    let saved = store
        .save_template(template("notes", "[spec]"), false)
        .await;
    let id =
        ApplicationId::parse(&saved.application_id).expect("the saved application has a valid ID");
    let state = async || {
        let id = id.as_str();
        let row = "SELECT generation,updated_at_ms,(SELECT COUNT(*) FROM events \
            WHERE kind='application_renamed') FROM applications WHERE id=?1";
        sqlx::query_as::<_, (i64, i64, i64)>(row)
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .expect("the application is read")
    };
    let before = state().await;
    store.refuse_audit(true).await;
    let request = crate::testing::audit_request("PUT /api/v1/applications/{id}/name");
    let author = Author {
        actor: Actor::Daemon,
        request: Some(&request),
    };
    let unchanged = crate::testing::rename_application(&id, "notes");
    let accepted = store.accept(author, unchanged, None, false, None).await;
    assert!(accepted.is_ok() && !request.recorded.get());
    store.refuse_audit(false).await;
    assert_eq!(state().await, before);
    assert_eq!(store.audit_actions().await, Vec::<String>::new());
}
