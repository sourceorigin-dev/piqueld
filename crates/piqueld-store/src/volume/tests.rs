use super::*;
use crate::testing::{prebuilt, pulled, target};
use crate::{Actor, Mutation, MutationResponse};
use piqueld_core::manifest::ApplicationTemplate;

/// Saves `notes`, whose `web` service mounts the volume `data`, with a
/// `staging` environment next to `production`, returning both.
async fn notes(store: &Store) -> (EnvironmentId, EnvironmentId) {
    let template = ApplicationTemplate::from(&prebuilt("app-notes-test"));
    let saved = store
        .save_template(template.to_manifest().validate_template().unwrap(), false)
        .await;
    let application = ApplicationId::parse(saved.application_id.as_str()).unwrap();
    let create = Mutation::CreateEnvironment {
        application: application.clone(),
        name: EnvironmentName::parse("staging").unwrap(),
        source: None,
    };
    let Ok(MutationResponse::Environment(staging)) = store.force(create).await else {
        panic!("staging created")
    };
    (store.production(&application).await, staging.id)
}

/// Deploys `environment` and saves its prepared target, returning the
/// target as saved.
async fn deploy(store: &Store, environment: &EnvironmentId) -> ResolvedApplication {
    let operation = store.request_deploy(environment, None).await.unwrap();
    store.mark_running(&operation.id).await;
    let rendering = store
        .deployment_snapshot(&operation.id)
        .await
        .unwrap()
        .rendering
        .unwrap();
    let compiled = target(
        &rendering.application,
        environment,
        store.instance_id().as_str(),
        pulled('a'),
    );
    store.save_prepared(&operation, &compiled).await.unwrap()
}

/// Deletes `environment` as the controller does once its runtime is gone.
async fn delete(store: &Store, environment: &EnvironmentId) {
    let deletion = store.request_delete(environment).await.unwrap();
    store.mark_running(&deletion.id).await;
    store.finish_delete_operation(&deletion).await.unwrap();
}

fn data() -> VolumeName {
    VolumeName::parse("data").unwrap()
}

#[tokio::test]
async fn saving_binds_a_new_volume_once_and_mounts_it() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    let first = deploy(&store, &production).await;
    let [volume] = store
        .environment_volumes(&production)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(volume.docker_name, DockerVolumeName::for_id(&volume.id));
    assert_eq!(volume.state, VolumeState::Attached);
    assert_eq!(volume.binding.as_ref().unwrap().name, data());
    assert_eq!(first.volumes[0].name, volume.docker_name);
    assert_eq!(first.services[0].mounts[0].volume_name, volume.docker_name);
    assert_eq!(first.volumes[0].owner(), Some(production.clone()));

    let second = deploy(&store, &production).await;
    assert_eq!(second.volumes, first.volumes);
    let staged = deploy(&store, &staging).await;
    assert_ne!(staged.volumes[0].name, volume.docker_name);
    assert_eq!(store.volumes(false).await.unwrap().len(), 2);
}

#[tokio::test]
async fn deleting_an_environment_detaches_its_volumes_for_another_to_attach() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &staging).await;
    let [volume] = store
        .environment_volumes(&staging)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(store.preview_volumes(&staging).await.unwrap(), []);
    assert!(matches!(
        store.delete_volume(Actor::Daemon, &volume.id).await,
        Err(StoreError::Volume(VolumeError::Attached { .. }))
    ));

    delete(&store, &staging).await;
    let [detached] = store.volumes(true).await.unwrap().try_into().unwrap();
    assert_eq!(detached.id, volume.id);
    assert_eq!(detached.state, VolumeState::Detached);
    let from = detached.detached_from.unwrap();
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
    let other = VolumeName::parse("restored").unwrap();
    let attached = store
        .attach_volume(Actor::Daemon, &production, &other, &volume.id)
        .await
        .unwrap();
    assert_eq!(attached.binding.unwrap().name, other);
    assert_eq!(attached.detached_from, None);
    assert_eq!(store.volumes(true).await.unwrap(), []);
}

#[tokio::test]
async fn a_rebound_name_mounts_its_new_volume_labelled_by_its_creator() {
    let (_temp, store) = Store::temporary().await;
    let (production, staging) = notes(&store).await;
    deploy(&store, &staging).await;
    let [old] = store
        .environment_volumes(&staging)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    delete(&store, &staging).await;
    let replaced = deploy(&store, &production).await.volumes[0].name.clone();

    sqlx::query("DELETE FROM volume_bindings WHERE name='data'")
        .execute(&store.pool)
        .await
        .unwrap();
    store
        .attach_volume(Actor::Daemon, &production, &data(), &old.id)
        .await
        .unwrap();
    let rebound = deploy(&store, &production).await;
    assert_eq!(rebound.volumes[0].name, old.docker_name);
    assert_eq!(rebound.services[0].mounts[0].volume_name, old.docker_name);
    assert_eq!(rebound.volumes[0].owner(), Some(staging));
    let [detached] = store.volumes(true).await.unwrap().try_into().unwrap();
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
        .unwrap()
        .try_into()
        .unwrap();
    delete(&store, &staging).await;
    let orphan = ManagedVolume {
        name: DockerVolumeName::parse("piqueld-orphan").unwrap(),
        owner: EnvironmentId::parse("env-deleted-long-ago").unwrap(),
        size_bytes: None,
    };
    let measured = ManagedVolume {
        name: attached.docker_name.clone(),
        owner: production.clone(),
        size_bytes: Some(4096),
    };
    store
        .record_volume_survey(&[measured, orphan.clone()])
        .await
        .unwrap();

    let attached = store.volume(&attached.id).await.unwrap();
    assert_eq!(attached.size_bytes, Some(4096));
    assert!(attached.measured_at_ms.is_some());
    let [adopted] = store.volumes(true).await.unwrap().try_into().unwrap();
    assert_eq!(adopted.docker_name, orphan.name);
    assert_eq!((adopted.detached_from, adopted.size_bytes), (None, None));
    let removed = store
        .delete_volume(Actor::Daemon, &adopted.id)
        .await
        .unwrap();
    assert_eq!(
        (removed.docker_name, removed.owner),
        (orphan.name, orphan.owner)
    );
}

#[tokio::test]
async fn migration_adopts_targets_volumes_and_the_preview_inventory() {
    let temp = tempfile::tempdir().unwrap();
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
    .unwrap();
    pool.close().await;
    let store = Store::open_unsynced(temp.path().join("db")).await.unwrap();
    let production = EnvironmentId::parse("env-production").unwrap();
    let [bound] = store
        .environment_volumes(&production)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        bound.docker_name.as_str(),
        "piqueld-env-production-volume-data-1"
    );
    assert_eq!(bound.binding.unwrap().name, data());
    assert_eq!(bound.created_at_ms, 3);
    let preview = EnvironmentId::parse("env-preview").unwrap();
    let [inventory] = store
        .preview_volumes(&preview)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        inventory.docker_name.as_str(),
        "piqueld-env-preview-volume-cache-2"
    );
    let detached = store.volume(&inventory.id).await.unwrap();
    let from = detached.detached_from.unwrap();
    assert_eq!(
        (from.application.as_str(), from.environment.as_str()),
        ("shop", "shop-feat")
    );
}
