use super::*;
use crate::testing::{credential, template, user};
use crate::{Actor, Mutation, MutationResponse};
use crate::{CredentialKind, NewCredential};
use piqueld_core::access::{AppPermission, Preset};

/// The actor `alice`'s session (see `Store::seed_caller`) records as.
fn alice() -> piqueld_core::EventActor {
    piqueld_core::EventActor::Account {
        user_id: "alice".into(),
        credential_id: Some("alice".into()),
    }
}

/// Saves an empty application named `name`, returning its ID.
async fn application(store: &Store, name: &str) -> ApplicationId {
    let saved = store.save_template(template(name, "[spec]"), false).await;
    ApplicationId::parse(saved.application_id).unwrap()
}

/// Creating an application gives its creator's account matching grants
/// there, unless it was created through a scoped credential.
#[tokio::test]
async fn only_unscoped_creators_receive_creator_grants() {
    let (_temp, store) = Store::temporary().await;
    let blog = application(&store, "blog").await;
    let mut grants = Preset::Developer.grants(&Scope::one(blog.clone()));
    grants
        .grant(
            Permission::Global(piqueld_core::access::GlobalPermission::AppsCreate),
            &Scope::All,
        )
        .unwrap();
    store.seed_auth_user(&user("alice"), &grants).await;
    for (id, scoped) in [("token", Some(&grants)), ("session", None)] {
        let credential = NewCredential {
            grants: scoped,
            ..credential(id, CredentialKind::Cli)
        };
        store.insert_credential("alice", &credential).await.unwrap();
    }
    let create = |credential_id, name: &str| {
        let manifest = template(name, "[spec]");
        let actor = Actor::Account(Caller {
            credential_id,
            user_id: "alice",
        });
        let store = &store;
        async move {
            let mutation = Mutation::save(manifest, None, false);
            let (MutationResponse::Saved(saved), _) = store
                .accept(actor, mutation, Some(0), false, None)
                .await
                .unwrap()
            else {
                panic!("saved application");
            };
            ApplicationId::parse(saved.application_id).unwrap()
        }
    };
    let readable = |store: &Store| {
        let store = store.clone();
        async move {
            let mut db = store.pool.acquire().await.unwrap();
            Holder::User("alice")
                .grants(&mut db)
                .await
                .unwrap()
                .app_scope(AppPermission::Write)
        }
    };
    let by_token = create("token", "from-token").await;
    assert!(!readable(&store).await.contains(&by_token));
    let by_session = create("session", "from-session").await;
    assert!(readable(&store).await.contains(&by_session));
}

/// Mutations record their caller on the operation and its events,
/// including events written later about that operation and daemon actions
/// it requests. A runtime action keeps the actor it started under even if
/// the operation is restarted.
#[tokio::test]
async fn operations_and_their_events_record_the_caller() {
    use crate::Visibility;
    use piqueld_core::observability::EventFilter;
    let (_temp, store) = Store::temporary().await;
    let blog = application(&store, "blog").await;
    let actor = store.seed_caller("alice", &Grants::admin()).await;
    let deploy = Mutation::deploy(store.production(&blog).await);
    let (MutationResponse::Operation(operation), _) =
        store.accept(actor, deploy, None, true, None).await.unwrap()
    else {
        panic!("deployment");
    };
    let filter = EventFilter {
        operation_id: Some(operation.operation_id.clone()),
        ..EventFilter::default()
    };
    let accepted = store
        .filtered_events(&filter, &Visibility::ALL, None, 100)
        .await
        .unwrap()
        .items
        .len();
    store.mark_running(&operation.operation_id).await;
    let id = &operation.operation_id;
    let action = store
        .begin_action(Some(id), piqueld_core::Phase::EnsureService, None)
        .await;
    let action = action.unwrap();
    let shared = store
        .begin_daemon_action(Some(id), piqueld_core::Phase::IngressConfigureRoutes, None)
        .await;
    let shared = shared.unwrap();
    store.finish_action(&shared, None).await.unwrap();
    let only_shared = EventFilter {
        action_id: Some(shared.id.clone()),
        ..EventFilter::default()
    };
    let shared = store
        .filtered_events(&only_shared, &Visibility::ALL, None, 100)
        .await
        .unwrap()
        .items;
    assert_eq!(shared.len(), 2, "started and succeeded");
    assert!(
        shared
            .iter()
            .all(|event| event.application_id.is_none() && event.actor == Some(alice()))
    );
    let restart = "UPDATE operations SET actor_user_id='bob' WHERE id=?1";
    sqlx::query(restart)
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();
    store.finish_action(&action, None).await.unwrap();
    let events = store
        .filtered_events(&filter, &Visibility::ALL, None, 100)
        .await
        .unwrap()
        .items;
    assert!(events.len() > accepted, "{events:?}");
    for event in events {
        assert_eq!(event.actor, Some(alice()), "{}", event.kind);
    }
}

/// Environment changes record their caller too, including every
/// operation one mutation requests, like deleting each environment of an
/// application.
#[tokio::test]
async fn environment_mutations_record_the_caller() {
    use crate::Visibility;
    use piqueld_core::{EnvironmentName, observability::EventFilter};
    let (_temp, store) = Store::temporary().await;
    let blog = application(&store, "blog").await;
    let actor = store.seed_caller("alice", &Grants::admin()).await;
    let filter = EventFilter {
        application_id: Some(blog.to_string()),
        ..EventFilter::default()
    };
    let history = |store: &Store| {
        let (store, filter) = (store.clone(), filter.clone());
        async move {
            let page = store.filtered_events(&filter, &Visibility::ALL, None, 100);
            page.await.unwrap().items
        }
    };
    let before = history(&store).await.len();
    let names = ["production", "staging"].map(|name| EnvironmentName::parse(name).unwrap());
    let create = Mutation::CreateEnvironment {
        application: blog.clone(),
        name: names[1].clone(),
        source: None,
    };
    store.accept(actor, create, None, true, None).await.unwrap();
    let delete = Mutation::DeleteApplication {
        id: blog.clone(),
        environments: names.to_vec(),
    };
    let (MutationResponse::Deleted(deleted), _) =
        store.accept(actor, delete, None, true, None).await.unwrap()
    else {
        panic!("deletion");
    };
    assert_eq!(deleted.operations.len(), 2);
    for operation in &deleted.operations {
        let actor: Option<String> =
            sqlx::query_scalar("SELECT actor_user_id FROM operations WHERE id=?1")
                .bind(&operation.operation_id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(actor.as_deref(), Some("alice"));
    }
    let events = history(&store).await.split_off(before);
    assert!(
        events
            .iter()
            .any(|event| event.environment_id.is_some() && event.operation_id.is_none())
    );
    for event in events {
        assert_eq!(event.actor, Some(alice()), "{}", event.kind);
    }
}

/// Superseding someone else's operation records the new request's caller
/// on its own operation only; the superseded one keeps its requester.
#[tokio::test]
async fn superseded_operations_keep_their_caller() {
    let (_temp, store) = Store::temporary().await;
    let blog = application(&store, "blog").await;
    let mut operations = Vec::new();
    for name in ["alice", "bob"] {
        let actor = store.seed_caller(name, &Grants::admin()).await;
        let deploy = Mutation::deploy(store.production(&blog).await);
        let (MutationResponse::Operation(operation), _) =
            store.accept(actor, deploy, None, true, None).await.unwrap()
        else {
            panic!("deployment");
        };
        operations.push(operation.operation_id);
    }
    for (operation, name) in operations.iter().zip(["alice", "bob"]) {
        let actor: Option<String> =
            sqlx::query_scalar("SELECT actor_user_id FROM operations WHERE id=?1")
                .bind(operation)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(actor.as_deref(), Some(name));
    }
}

/// Deleting an application removes grants on it, and expires scoped
/// credentials that held grants only there a minute later instead of
/// leaving them with no access.
#[tokio::test]
async fn deleting_an_application_expires_tokens_left_without_grants() {
    let (_temp, store) = Store::temporary().await;
    let blog = application(&store, "blog").await;
    let shop = application(&store, "shop").await;
    store.seed_auth_user(&user("alice"), &Grants::admin()).await;
    let deploy_blog = Preset::Deploy.grants(&Scope::one(blog.clone()));
    let mut both = deploy_blog.clone();
    both.extend(&Preset::ReadOnly.grants(&Scope::one(shop.clone())));
    for (id, grants) in [
        ("blog-only", Some(&deploy_blog)),
        ("both", Some(&both)),
        ("session", None),
    ] {
        let credential = NewCredential {
            grants,
            ..credential(id, CredentialKind::Token)
        };
        store.insert_credential("alice", &credential).await.unwrap();
    }
    let delete = Mutation::DeleteApplication {
        id: blog.clone(),
        environments: Vec::new(),
    };
    let (MutationResponse::Deleted(deleted), _) = store
        .accept(Actor::Daemon, delete, None, true, None)
        .await
        .unwrap()
    else {
        panic!("deletion");
    };
    let operation = &deleted.operations[0];
    store.mark_running(&operation.operation_id).await;
    let operation = store.operation(&operation.operation_id).await.unwrap();
    store.finish_delete_operation(&operation).await.unwrap();
    let left = store.credential_owner("blog-only").await.unwrap().unwrap();
    assert!(left.grants.is_empty());
    let expires_at =
        sqlx::query_scalar!("SELECT expires_at FROM auth_credentials WHERE id='blog-only'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert!(expires_at.is_some_and(|at| at <= now_secs() + 60));
    assert!(store.credential_owner("session").await.unwrap().is_some());
    let both = store.credential_owner("both").await.unwrap().unwrap();
    assert_eq!(both.grants.app_scope(AppPermission::Read), Scope::one(shop));
    assert!(both.grants.app_scope(AppPermission::Deploy).is_empty());
}
