//! Ingress tests. Most run an [`Ingress`] against a stub Docker Engine
//! ([`Engine`]); the `ingress_caddy` tests drive a real gateway in the
//! isolated Docker harness ([`Scenario`]).
mod acme;
mod configuration;
mod dns01;
mod health;
mod networks;
mod replacement;
mod routing;
mod scenario;
mod traffic;
mod tunnel;
use super::*;
use crate::{
    api::{Actor, Mutation, MutationResponse},
    git::Sources,
    reconcile::Controller,
};
use hyper::Method;
use piqueld_core::{
    EnvironmentId, NormalizedApplication, OperationState, manifest::ApplicationTemplate,
};
use piqueld_docker::{BollardDocker, DockerApi};
use scenario::Scenario;
use serde_json::json;

/// Response body that must never reach history, diagnostics or health messages.
const PRIVATE_BODY: &str = "private-engine-detail";

/// A data directory and store for test ingresses, and the Docker Engine
/// socket in it, which a stub router answers or nothing serves.
pub(super) struct Engine {
    pub(super) directory: tempfile::TempDir,
    pub(super) store: Arc<Store>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Engine {
    async fn open() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open_unsynced(directory.path().join("db"))
                .await
                .unwrap(),
        );
        Self {
            directory,
            store,
            server: None,
        }
    }

    /// A socket nothing listens on: Docker clients require the file to
    /// exist, and every request fails at once.
    pub(super) async fn unused() -> Self {
        let engine = Self::open().await;
        drop(tokio::net::UnixListener::bind(engine.socket()).unwrap());
        engine
    }

    /// An engine answering every request with the router `serve` builds for
    /// this installation's store.
    pub(super) async fn serving(serve: impl FnOnce(&Store) -> axum::Router) -> Self {
        let mut engine = Self::open().await;
        let listener = tokio::net::UnixListener::bind(engine.socket()).unwrap();
        let router = serve(&engine.store);
        engine.server = Some(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
        engine
    }

    /// An engine failing every request with [`PRIVATE_BODY`].
    pub(super) async fn failing() -> Self {
        Self::serving(|_| {
            axum::Router::new()
                .fallback(|| async { (hyper::StatusCode::INTERNAL_SERVER_ERROR, PRIVATE_BODY) })
        })
        .await
    }

    pub(super) fn socket(&self) -> PathBuf {
        self.directory.path().join("docker.sock")
    }

    /// An ingress of this installation, `enabled` or not.
    pub(super) fn ingress(&self, enabled: bool) -> Ingress {
        Ingress::new(
            enabled,
            &self.socket(),
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .unwrap()
    }

    /// The history so far, which never holds [`PRIVATE_BODY`].
    async fn history(&self) -> Vec<piqueld_core::Event> {
        let events = self.store.events(None, None, 100).await.unwrap().items;
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains(PRIVATE_BODY)
        );
        events
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

/// `[ingress.private]`, enabled.
pub(super) fn private_config() -> piqueld_config::PrivateIngressConfig {
    piqueld_config::PrivateIngressConfig {
        enabled: true,
        ..Default::default()
    }
}

/// The routes of an application serving `host` with `visibility`.
pub(super) fn routes(visibility: Visibility, host: &str) -> Vec<ValidatedRoute> {
    let mut manifest = application("one", host, "body").to_manifest();
    manifest.spec.routes[0].visibility = visibility;
    manifest.validate().unwrap().spec().routes.clone()
}

/// An application `name` whose `web` service answers `body` for `host`.
/// Docker watches each new container for one second instead of the default
/// 30, so deployments replacing one finish without that wait.
pub(super) fn application(name: &str, host: &str, body: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(&format!("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[[spec.services]]\nname='web'\ncommand=['caddy']\narguments=['respond','--listen',':8080','--body','{body}']\nrollout={{monitor_seconds=1}}\n[spec.services.source]\ntype='image'\nimage='{CADDY_IMAGE}'\n[[spec.routes]]\nhostname='{host}'\nvisibility='public'\nservice='web'\nport=8080")).unwrap().normalize(piqueld_core::ApplicationId::parse("input-app").unwrap())
}

/// Saves `app` on behalf of `actor`, and deploys it if `deploy` is set.
/// Returns its production environment and the deployment's operation.
pub(super) async fn save(
    store: &Store,
    actor: Actor<'_>,
    app: &NormalizedApplication,
    deploy: bool,
) -> (EnvironmentId, Option<String>) {
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            actor,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(app)),
                expected_application_id: None,
                deploy,
            },
            None,
            true,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved")
    };
    let id = store
        .production(&piqueld_core::ApplicationId::parse(saved.application_id).unwrap())
        .await;
    (id, saved.operation_id)
}

/// Saves and deploys `app` on behalf of `actor`, returning its production
/// environment and the deployment's operation.
pub(super) async fn request_deployment(
    store: &Store,
    actor: Actor<'_>,
    app: NormalizedApplication,
) -> (EnvironmentId, String) {
    let (id, operation) = save(store, actor, &app, true).await;
    (id, operation.unwrap())
}

/// Requests a deployment of `app` and scans until it succeeds. A failed
/// operation fails the test: a deployment never needs a retry here.
async fn deploy(
    store: &Store,
    controller: &Controller<BollardDocker>,
    app: NormalizedApplication,
) -> EnvironmentId {
    let (id, operation) = request_deployment(store, Actor::Daemon, app).await;
    tokio::time::timeout(Duration::from_mins(3), async {
        loop {
            controller.scan(&CancellationToken::new()).await.unwrap();
            let status = store.operation(&operation).await.unwrap();
            if status.state == OperationState::Succeeded {
                break;
            }
            eprintln!("deploy: {:?} {:?}", status.state, status.error_message);
            assert_ne!(
                status.state,
                OperationState::Failed,
                "a deployment must not need a retry: {:?} {:?}",
                status.error_code,
                status.error_message
            );
            // wall-clock: each scan advances the deployment as far as the real
            // Swarm has converged, which signals nothing the scan could await.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("deployment converges");
    id
}

/// The loopback port the harness publishes in environment `variable`.
fn port(variable: &str) -> u16 {
    std::env::var(variable).unwrap().parse().unwrap()
}

/// One public route on `www.example.com` and one private route on
/// `admin.example.com`, in their own environments.
fn listener_table() -> RoutingTable {
    [
        (
            EnvironmentId::parse("env-public").unwrap(),
            routes(Visibility::Public, "www.example.com"),
        ),
        (
            EnvironmentId::parse("env-private").unwrap(),
            routes(Visibility::Private, "admin.example.com"),
        ),
    ]
    .into()
}

/// The edge network's subnet, whose peers' PROXY headers the private
/// listener trusts.
fn edge() -> [String; 1] {
    ["172.20.0.0/16".to_owned()]
}

/// An ingress with private ingress enabled whose Docker Engine is never called.
async fn private_ingress() -> (Engine, Ingress) {
    let engine = Engine::unused().await;
    let ingress = engine.ingress(true).with_private(&private_config());
    (engine, ingress)
}
