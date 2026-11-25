use super::*;
use crate::api::{Mutation, MutationResponse};
use piqueld_docker::BollardDocker;
use piqueld_store::Store;

/// A store in a temporary directory, and a Docker socket nothing serves.
struct Harness {
    directory: tempfile::TempDir,
    store: Arc<Store>,
    sources: Arc<crate::git::Sources>,
    socket: std::path::PathBuf,
    _listener: tokio::net::UnixListener,
}

impl Harness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open_unsynced(directory.path().join("db"))
                .await
                .unwrap(),
        );
        let sources = Arc::new(crate::git::Sources::open(directory.path()).await.unwrap());
        let socket = directory.path().join("unused.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        Self {
            directory,
            store,
            sources,
            socket,
            _listener: listener,
        }
    }

    /// A controller of this store whose Docker Engine never answers.
    fn controller(&self) -> Controller<BollardDocker> {
        Controller::new(
            Arc::new(BollardDocker::connect(&self.socket).unwrap()),
            Arc::clone(&self.store),
            Arc::clone(&self.sources),
        )
    }
}

/// Saves and deploys application `name` with `spec`, a TOML `[spec]`
/// table, returning its requested operation.
async fn deploy(store: &Store, name: &str, spec: &str) -> Operation {
    let manifest = piqueld_core::manifest::parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n{spec}"
    ))
    .unwrap();
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            crate::api::Actor::Daemon,
            Mutation::save(manifest, None, true),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved deployment")
    };
    store.operation(&saved.operation_id.unwrap()).await.unwrap()
}

/// Accepts a routed environment and starts executing its operation.
async fn running_operation(store: &Store) -> Operation {
    let spec = format!(
        "[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='{}'\n\
         [[spec.routes]]\nhostname='routed.example.com'\nservice='web'\nport=80",
        crate::ingress::CADDY_IMAGE
    );
    let operation = deploy(store, "routed", &spec).await;
    store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store.operation(&operation.id).await.unwrap()
}

#[tokio::test]
async fn routing_superseded_while_waiting_for_the_gateway_is_superseded() {
    let harness = Harness::new().await;
    let store = &harness.store;
    let operation = running_operation(store).await;
    let routes = store
        .deployment_snapshot(&operation.id)
        .await
        .unwrap()
        .rendering
        .unwrap()
        .application
        .spec()
        .routes
        .clone();
    let ingress = Arc::new(
        crate::ingress::Ingress::new(
            true,
            &harness.socket,
            harness.directory.path(),
            Arc::clone(store),
        )
        .unwrap(),
    );
    let controller = Arc::new(harness.controller().with_ingress(Arc::clone(&ingress)));

    let gateway = ingress.hold_updates().await;
    let staging = tokio::spawn({
        let operation = operation.clone();
        async move { controller.sync_routes(&operation, &routes, true).await }
    });
    store
        .wait_until(async || {
            store.operation(&operation.id).await.unwrap().phase == Some(Phase::Routing)
        })
        .await;
    store
        .accept(
            crate::api::Actor::Daemon,
            Mutation::deploy(operation.environment_id.clone()),
            None,
            true,
            None,
        )
        .await
        .unwrap();
    drop(gateway);
    assert!(matches!(
        staging.await.unwrap(),
        Err(OperationError::Superseded)
    ));
}

#[tokio::test]
async fn reconciliation_leaves_concurrent_daemon_actions_open() {
    let harness = Harness::new().await;
    let store = &harness.store;
    let operation = running_operation(store).await;
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Requested,
            None,
        )
        .await
        .unwrap();
    let controller = harness.controller();
    let action = store
        .begin_action(None, Phase::IngressStartGateway, None)
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let run = controller.run(
        Arc::new(tokio::sync::Notify::new()),
        std::time::Duration::from_mins(1),
        0,
        0,
        cancellation.clone(),
    );
    let finished = async {
        store
            .wait_until(async || {
                store.operation(&operation.id).await.unwrap().state != OperationState::Requested
            })
            .await;
        let finished = store.finish_action(&action, None).await;
        cancellation.cancel();
        finished
    };
    let (result, finished) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run, finished)
    })
    .await
    .unwrap();
    result.unwrap();
    finished.unwrap();
}

/// Deleting a secret waits for Docker mutations in progress before it
/// checks that nothing uses the secret.
#[tokio::test]
async fn secret_deletion_waits_for_docker_mutations() {
    let harness = Harness::new().await;
    let store = &harness.store;
    let operation = deploy(store, "secrets", "[spec]").await;
    let environment = &operation.environment_id;
    let daemon = crate::api::Actor::Daemon;
    store
        .put_secret(daemon, environment, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let controller = harness.controller();
    let service = crate::api::ApplicationService::new(
        Arc::clone(store),
        controller.runtime(Arc::default()),
        Arc::clone(&harness.sources),
    )
    .unwrap();
    let deleting = async || store.secrets(environment).await.unwrap()[0].deleting;
    let guard = controller.mutations.lock().await;
    let deletion = service.delete_secret(daemon, environment, "token", 1);
    tokio::pin!(deletion);
    let wait = std::time::Duration::from_millis(200);
    tokio::time::timeout(wait, &mut deletion)
        .await
        .expect_err("`tokio::time::timeout` should be rejected");
    assert!(!deleting().await);
    drop(guard);
    tokio::time::timeout(wait, &mut deletion)
        .await
        .expect_err("`tokio::time::timeout` should be rejected");
    assert!(deleting().await);
}

#[tokio::test]
async fn execution_cleanup_waits_for_live_repair_to_commit() {
    let harness = Harness::new().await;
    let store = &harness.store;
    let operation = deploy(store, "repair", "[spec]").await;
    store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    let controller = harness.controller();
    let abandoned = store
        .begin_action(Some(&operation.id), Phase::ResolveImage, Some("web"))
        .await
        .unwrap();
    let guard = controller.mutations.lock().await;
    let repair = store
        .begin_action(Some(&operation.id), Phase::EnsureService, Some("web"))
        .await
        .unwrap();
    store.action_request(&repair, 1).await.unwrap();
    let cancellation = CancellationToken::new();
    let cleanup = controller.run_operation(&operation, &cancellation);
    tokio::pin!(cleanup);
    tokio::time::timeout(std::time::Duration::from_millis(100), &mut cleanup)
        .await
        .expect_err("`tokio::time::timeout` should be rejected");
    store.finish_action(&repair, None).await.unwrap();
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), cleanup)
        .await
        .unwrap()
        .unwrap();
    let events = store
        .events(Some(&operation.environment_id), None, 100)
        .await
        .unwrap()
        .items;
    assert!(
        events
            .iter()
            .any(|e| e.action_id.as_deref() == Some(&repair.id) && e.kind == "action_succeeded")
    );
    assert!(
        !events
            .iter()
            .any(|e| e.action_id.as_deref() == Some(&repair.id)
                && e.kind == "action_outcome_unknown")
    );
    assert!(events.iter().any(
        |e| e.action_id.as_deref() == Some(&abandoned.id) && e.kind == "action_outcome_unknown"
    ));
}
