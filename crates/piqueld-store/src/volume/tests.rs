use super::*;
use crate::testing::{prebuilt, pulled, target};
use crate::{Actor, Mutation, MutationResponse};
use piqueld_core::manifest::ApplicationTemplate;

/// Saves `notes`, whose `web` service mounts the volume `data`, with a
/// `staging` environment next to `production`, returning both.
async fn notes(store: &Store) -> (EnvironmentId, EnvironmentId) {
    let template = ApplicationTemplate::from(&prebuilt("app-notes-test"));
    let saved = store
        .save_template(
            template
                .to_manifest()
                .validate_template()
                .expect("validate template succeeds"),
            false,
        )
        .await;
    let application =
        ApplicationId::parse(saved.application_id.as_str()).expect("the value parses");
    let production = store.production(&application).await;
    (
        production.clone(),
        create(store, &production, "staging").await,
    )
}

/// Adds the environment `name`, never deployed, to `sibling`'s application.
async fn create(store: &Store, sibling: &EnvironmentId, name: &str) -> EnvironmentId {
    let create = Mutation::CreateEnvironment {
        application: store
            .get(sibling)
            .await
            .expect("get succeeds")
            .environment
            .application_id,
        name: EnvironmentName::parse(name).expect("the value parses"),
        source: None,
    };
    let Ok(MutationResponse::Environment(created)) = store.force(create).await else {
        panic!("{name} created")
    };
    created.id
}

/// Deploys `environment` and saves its prepared target, returning the
/// target as saved.
async fn deploy(store: &Store, environment: &EnvironmentId) -> ResolvedApplication {
    let operation = store
        .request_deploy(environment, None)
        .await
        .expect("request deploy succeeds");
    store.mark_running(&operation.id).await;
    let rendering = store
        .deployment_snapshot(&operation.id)
        .await
        .expect("deployment snapshot succeeds")
        .rendering
        .expect("the deployment is rendered");
    let compiled = target(
        &rendering.application,
        environment,
        store.instance_id().as_str(),
        pulled('a'),
    );
    store
        .save_prepared(&operation, &compiled)
        .await
        .expect("save prepared succeeds")
}

/// Deletes `environment` as the controller does once its runtime is gone.
async fn delete(store: &Store, environment: &EnvironmentId) {
    let deletion = store
        .request_delete(environment)
        .await
        .expect("request delete succeeds");
    store.mark_running(&deletion.id).await;
    store
        .finish_delete_operation(&deletion)
        .await
        .expect("finish delete operation succeeds");
}

fn data() -> VolumeName {
    VolumeName::parse("data").expect("the value parses")
}

#[tokio::test]
async fn saving_binds_a_new_volume_once_and_mounts_it() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    let first = deploy(&store, &production).await;
    let [volume] = store
        .environment_volumes(&production)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(volume.docker_name, DockerVolumeName::for_id(&volume.id));
    assert_eq!(volume.state, VolumeState::Attached);
    assert_eq!(
        volume
            .binding
            .as_ref()
            .expect("the volume is attached")
            .name,
        data()
    );
    assert_eq!(first.volumes[0].name, volume.docker_name);
    assert_eq!(first.services[0].mounts[0].volume_name, volume.docker_name);
    assert_eq!(first.volumes[0].owner(), Some(production.clone()));

    let second = deploy(&store, &production).await;
    assert_eq!(second.volumes, first.volumes);
    let staged = deploy(&store, &staging).await;
    assert_ne!(staged.volumes[0].name, volume.docker_name);
    assert_eq!(
        store.volumes(false).await.expect("volumes succeeds").len(),
        2
    );
}

#[tokio::test]
async fn deleting_an_environment_detaches_its_volumes_for_another_to_attach() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &staging).await;
    let [volume] = store
        .environment_volumes(&staging)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(
        store
            .preview_volumes(&staging)
            .await
            .expect("preview volumes succeeds"),
        []
    );
    assert!(matches!(
        store.delete_volume(Actor::Daemon, &volume.id).await,
        Err(StoreError::Volume(VolumeError::Attached { .. }))
    ));

    delete(&store, &staging).await;
    let [detached] = store
        .volumes(true)
        .await
        .expect("volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(detached.id, volume.id);
    assert_eq!(detached.state, VolumeState::Detached);
    let from = detached
        .detached_from
        .expect("the volume records where it came from");
    assert_eq!(
        (
            from.application.as_str(),
            from.environment.as_str(),
            from.name
        ),
        ("notes", "staging", Some(data()))
    );

    deploy(&store, &production).await;
    assert!(matches!(
        store
            .attach_volume(Actor::Daemon, &production, &data(), &volume.id)
            .await,
        Err(StoreError::Volume(VolumeError::NameBound { .. }))
    ));
    let qa = create(&store, &production, "qa").await;
    let undeclared = VolumeName::parse("restored").expect("the value parses");
    assert!(matches!(
        store
            .attach_volume(Actor::Daemon, &qa, &undeclared, &volume.id)
            .await,
        Err(StoreError::Volume(VolumeError::NameUndeclared { .. }))
    ));
    let attached = store
        .attach_volume(Actor::Daemon, &qa, &data(), &volume.id)
        .await
        .expect("attach volume succeeds");
    assert_eq!(
        attached
            .binding
            .expect("the volume is attached")
            .environment_id,
        qa
    );
    assert_eq!(attached.detached_from, None);
    assert_eq!(store.volumes(true).await.expect("volumes succeeds"), []);
    assert_eq!(
        deploy(&store, &qa).await.volumes[0].name,
        volume.docker_name
    );
}

#[tokio::test]
async fn a_rebound_name_mounts_its_new_volume_labelled_by_its_creator() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &staging).await;
    let [old] = store
        .environment_volumes(&staging)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    delete(&store, &staging).await;
    let replaced = deploy(&store, &production).await.volumes[0].name.clone();

    sqlx::query("DELETE FROM volume_bindings WHERE name='data'")
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    store
        .attach_volume(Actor::Daemon, &production, &data(), &old.id)
        .await
        .expect("attach volume succeeds");
    let rebound = deploy(&store, &production).await;
    assert_eq!(rebound.volumes[0].name, old.docker_name);
    assert_eq!(rebound.services[0].mounts[0].volume_name, old.docker_name);
    assert_eq!(rebound.volumes[0].owner(), Some(staging));
    let [detached] = store
        .volumes(true)
        .await
        .expect("volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(detached.docker_name, replaced);
}

#[tokio::test]
async fn a_survey_adopts_unknown_volumes_records_sizes_and_forgets_gone_detached_ones() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &production).await;
    deploy(&store, &staging).await;
    let [attached] = store
        .environment_volumes(&production)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    delete(&store, &staging).await;
    let orphan = ManagedVolume {
        name: DockerVolumeName::parse("piqueld-orphan").expect("the value parses"),
        owner: EnvironmentId::parse("env-deleted-long-ago").expect("the value parses"),
    };
    let listed = ManagedVolume {
        name: attached.docker_name.clone(),
        owner: production.clone(),
    };
    store
        .record_volume_survey(&[listed, orphan.clone()], now_ms())
        .await
        .expect("record volume survey succeeds");
    let measured = |size| BTreeMap::from([(attached.docker_name.to_string(), size)]);
    store
        .record_volume_sizes(&measured(4096))
        .await
        .expect("record volume sizes succeeds");
    store
        .record_volume_sizes(&BTreeMap::new())
        .await
        .expect("record volume sizes succeeds");

    let attached = store.volume(&attached.id).await.expect("volume succeeds");
    assert_eq!(attached.size_bytes, Some(4096));
    assert!(attached.measured_at_ms.is_some());
    let [adopted] = store
        .volumes(true)
        .await
        .expect("volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(adopted.docker_name, orphan.name);
    assert_eq!((adopted.detached_from, adopted.size_bytes), (None, None));
    let removed = store
        .delete_volume(Actor::Daemon, &adopted.id)
        .await
        .expect("delete volume succeeds");
    assert_eq!(
        (removed.docker_name, removed.owner),
        (orphan.name, orphan.owner)
    );
}

/// While Docker removes a deleted volume, it is hidden, can't be attached,
/// and a survey still listing it doesn't adopt it again; a failed removal
/// keeps it under its ID, and a finished one forgets it.
#[tokio::test]
async fn a_volume_being_removed_is_hidden_until_docker_removed_it() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &staging).await;
    let [volume] = store
        .environment_volumes(&staging)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    delete(&store, &staging).await;
    let qa = create(&store, &production, "qa").await;
    let removing = store
        .delete_volume(Actor::Daemon, &volume.id)
        .await
        .expect("delete volume succeeds");
    let listed = ManagedVolume {
        name: removing.docker_name.clone(),
        owner: removing.owner.clone(),
    };
    store
        .record_volume_survey(&[listed], now_ms())
        .await
        .expect("record volume survey succeeds");
    assert_eq!(store.volumes(false).await.expect("volumes succeeds"), []);
    assert!(matches!(
        store
            .attach_volume(Actor::Daemon, &qa, &data(), &volume.id)
            .await,
        Err(StoreError::NotFound)
    ));

    store
        .finish_volume_removal(&volume.id, false)
        .await
        .expect("finish volume removal succeeds");
    assert_eq!(
        store.volume(&volume.id).await.expect("volume succeeds").id,
        volume.id
    );
    store
        .delete_volume(Actor::Daemon, &volume.id)
        .await
        .expect("delete volume succeeds");
    store
        .finish_volume_removal(&volume.id, true)
        .await
        .expect("finish volume removal succeeds");
    assert!(matches!(
        store.volume(&volume.id).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_survey_keeps_detached_volumes_recorded_after_its_listing() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    let listed_at = now_ms();
    deploy(&store, &production).await;
    deploy(&store, &staging).await;
    delete(&store, &staging).await;
    store
        .record_volume_survey(&[], listed_at)
        .await
        .expect("record volume survey succeeds");
    assert_eq!(
        store.volumes(true).await.expect("volumes succeeds").len(),
        1
    );
    store
        .record_volume_survey(&[], now_ms())
        .await
        .expect("record volume survey succeeds");
    assert_eq!(store.volumes(true).await.expect("volumes succeeds"), []);
    assert_eq!(
        store
            .environment_volumes(&production)
            .await
            .expect("environment volumes succeeds")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_live_previews_volume_stays_its_own_until_it_is_deleted() {
    let (_temp, store) = Store::temporary().await;
    let (production, preview) = notes(&store).await;
    deploy(&store, &preview).await;
    let [leftover] = store
        .environment_volumes(&preview)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    let deletion = store
        .request_delete(&preview)
        .await
        .expect("request delete succeeds");
    store.mark_running(&deletion.id).await;
    sqlx::query("UPDATE environments SET kind='preview',branch='feat' WHERE id=?1")
        .bind(preview.as_str())
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    sqlx::query("DELETE FROM volume_bindings WHERE environment_id=?1")
        .bind(preview.as_str())
        .execute(&store.pool)
        .await
        .expect("execute succeeds");
    let qa = create(&store, &production, "qa").await;
    assert!(matches!(
        store
            .attach_volume(Actor::Daemon, &qa, &data(), &leftover.id)
            .await,
        Err(StoreError::Volume(VolumeError::Preview))
    ));

    let [removed] = store
        .preview_volumes(&preview)
        .await
        .expect("preview volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(removed.id, leftover.id);
    store
        .finish_delete_operation(&deletion)
        .await
        .expect("finish delete operation succeeds");
    assert!(matches!(
        store.volume(&leftover.id).await,
        Err(StoreError::NotFound)
    ));
}

/// Environments that follow a branch or receive promotions bind every name
/// their manifest declares when they deploy it, so they take any unbound
/// name, even with a manifest of their own that doesn't declare it.
#[tokio::test]
async fn branch_and_promoted_environments_take_any_unbound_name() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    let mut detached = Vec::new();
    for name in ["old-a", "old-b"] {
        let environment = create(&store, &production, name).await;
        deploy(&store, &environment).await;
        let [volume] = store
            .environment_volumes(&environment)
            .await
            .expect("environment volumes succeeds")
            .try_into()
            .expect("exactly one volume");
        delete(&store, &environment).await;
        detached.push(volume.id);
    }
    let tracking = create(&store, &production, "tracking").await;
    for (environment, branch, promoted_from) in [
        (&tracking, Some("main"), None),
        (&staging, None, Some(production.as_str())),
    ] {
        sqlx::query("UPDATE environments SET branch=?2,promoted_from=?3,manifest_json=(SELECT desired_json FROM applications WHERE id=environments.application_id) WHERE id=?1")
            .bind(environment.as_str())
            .bind(branch)
            .bind(promoted_from)
            .execute(&store.pool)
            .await
            .expect("execute succeeds");
    }
    let undeclared = VolumeName::parse("restored").expect("the value parses");
    for (environment, volume) in [&tracking, &staging].into_iter().zip(&detached) {
        assert!(
            store
                .get(environment)
                .await
                .expect("get succeeds")
                .manifest()
                .is_some()
        );
        let attached = store
            .attach_volume(Actor::Daemon, environment, &undeclared, volume)
            .await
            .expect("attach volume succeeds");
        assert_eq!(
            attached.binding.expect("the volume is attached").name,
            undeclared
        );
    }
}

#[tokio::test]
async fn migration_adopts_targets_volumes_and_the_preview_inventory() {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let pool = Store::migrated(&temp.path().join("db"), 8).await;
    sqlx::raw_sql(
        r#"INSERT INTO applications(id,name,desired_json,created_at_ms,updated_at_ms) VALUES('app-shop-test','shop','{}',1,1);
        INSERT INTO environments(id,application_id,name,created_at_ms,updated_at_ms) VALUES('env-production','app-shop-test','production',1,1);
        INSERT INTO environments(id,application_id,name,kind,branch,created_at_ms,updated_at_ms) VALUES('env-preview','app-shop-test','shop-feat','preview','feat',2,2);
        INSERT INTO operations(id,environment_id,kind,state,generation,created_at_ms,updated_at_ms,target_json) VALUES('op-1','env-production','apply','running',1,3,3,'{"volumes":[{"logical_name":"data","name":"piqueld-env-production-volume-data-1","labels":{}}]}');
        INSERT INTO preview_volumes(environment_id,name,created_at_ms) VALUES('env-preview','piqueld-env-preview-volume-cache-2',4);"#,
    )
    .execute(&pool)
    .await
    .expect("execute succeeds");
    pool.close().await;
    let store = Store::open_unsynced(temp.path().join("db"))
        .await
        .expect("open unsynced succeeds");
    let production = EnvironmentId::parse("env-production").expect("the value parses");
    let [bound] = store
        .environment_volumes(&production)
        .await
        .expect("environment volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(
        bound.docker_name.as_str(),
        "piqueld-env-production-volume-data-1"
    );
    assert_eq!(bound.binding.expect("the volume is attached").name, data());
    assert_eq!(bound.created_at_ms, 3);
    let preview = EnvironmentId::parse("env-preview").expect("the value parses");
    let [inventory] = store
        .preview_volumes(&preview)
        .await
        .expect("preview volumes succeeds")
        .try_into()
        .expect("exactly one volume");
    assert_eq!(
        inventory.docker_name.as_str(),
        "piqueld-env-preview-volume-cache-2"
    );
    let detached = store.volume(&inventory.id).await.expect("volume succeeds");
    let from = detached
        .detached_from
        .expect("the volume records where it came from");
    assert_eq!(
        (from.application.as_str(), from.environment.as_str()),
        ("shop", "shop-feat")
    );
}
