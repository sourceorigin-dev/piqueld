//! A gateway in the isolated Docker harness, with one application deployed,
//! which the `ingress_caddy` tests take through their steps.
use super::*;

/// The gateway's internal CA, below the scenario's data directory.
pub(super) const ROOT_CERTIFICATE: &str = "ingress/data/caddy/pki/authorities/local/root.crt";

pub(super) struct Scenario {
    pub(super) directory: tempfile::TempDir,
    socket: PathBuf,
    pub(super) store: Arc<Store>,
    pub(super) docker: Arc<BollardDocker>,
    pub(super) gateway: Arc<Ingress>,
    pub(super) controller: Controller<BollardDocker>,
    pub(super) first: EnvironmentId,
    /// The gateway's internal CA, which issues its certificates.
    pub(super) root: reqwest::Certificate,
    pub(super) client: reqwest::Client,
    pub(super) tls_port: u16,
    pub(super) plain_port: u16,
    /// Stops counting the gateway's traffic, then flushes the counts.
    pub(super) counting: CancellationToken,
    /// The traffic counter, until a step stops it.
    pub(super) counter: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Scenario {
    pub(super) async fn new() -> Self {
        assert_eq!(std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(), Ok("1"));
        let socket =
            PathBuf::from(std::env::var("PIQUELD_DOCKER_SOCKET").expect("the variable is set"));
        assert_ne!(
            std::fs::canonicalize(&socket).expect("the path resolves"),
            PathBuf::from("/run/docker.sock")
        );
        let root =
            PathBuf::from(std::env::var("PIQUELD_DOCKER_DATA_DIR").expect("the variable is set"));
        let directory = tempfile::tempdir_in(root).expect("the temp dir is created");
        let store = Arc::new(
            Store::open_unsynced(directory.path().join("db"))
                .await
                .expect("the store opens"),
        );
        let docker = Arc::new(BollardDocker::connect(&socket).expect("the connection opens"));
        docker
            .ensure_swarm(true)
            .await
            .expect("ensure swarm succeeds");
        let mut gateway = Ingress::new(true, &socket, directory.path(), Arc::clone(&store))
            .expect("the value is valid");
        gateway.issuer = Some(json!({"module":"internal"}));
        gateway.extra_hosts = vec![
            "http-acme.example.test:127.0.0.1".into(),
            "alpn-acme.example.test:127.0.0.1".into(),
        ];
        gateway.private_port = Some(port("PIQUELD_INGRESS_PRIVATE_PORT"));
        gateway.traffic = true;
        let gateway = Arc::new(gateway);
        let counting = CancellationToken::new();
        let traffic = gateway
            .bind_traffic()
            .await
            .expect("bind traffic succeeds")
            .expect("the listener is enabled");
        let counter = tokio::spawn(traffic.run(counting.clone()));
        let sources = Arc::new(
            Sources::open(directory.path())
                .await
                .expect("the store opens"),
        );
        let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store), sources)
            .with_ingress(Arc::clone(&gateway));
        let first = deploy(
            &store,
            &controller,
            application("one", "one.example.test", "first backend"),
        )
        .await;
        let root = tokio::fs::read(directory.path().join(ROOT_CERTIFICATE))
            .await
            .expect("read succeeds");
        let root = reqwest::Certificate::from_pem(&root).expect("from pem succeeds");
        let client = Self::trusting(
            &root,
            &[
                "one.example.test",
                "two.example.test",
                "www.example.test",
                "three.example.test",
                "four.example.test",
            ],
        )
        .timeout(Duration::from_secs(3))
        .http1_only()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("the client builds");
        let scenario = Self {
            directory,
            socket,
            store,
            docker,
            gateway,
            controller,
            first,
            root,
            client,
            tls_port: port("PIQUELD_INGRESS_HTTPS_PORT"),
            plain_port: port("PIQUELD_INGRESS_HTTP_PORT"),
            counting,
            counter: Mutex::new(Some(counter)),
        };
        scenario.await_certificate("one.example.test").await;
        scenario
    }

    /// Runs `step` on a new scenario, then `cleanup`, and stops the gateway
    /// even when the step panics, so the next harness test starts without
    /// one. A panicking step relays the gateway's logs first.
    pub(super) async fn run(step: impl AsyncFnOnce(&Self), cleanup: impl AsyncFnOnce(&Self)) {
        use futures_util::FutureExt;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "an earlier test in this process may have installed the subscriber"
        )]
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let scenario = Box::pin(Self::new()).await;
        let result = Box::pin(std::panic::AssertUnwindSafe(step(&scenario)).catch_unwind()).await;
        if result.is_err()
            && let Err(error) = scenario.gateway.relay_logs().await
        {
            eprintln!("could not relay the gateway's logs: {error:#}");
        }
        cleanup(&scenario).await;
        scenario
            .gateway
            .stop_gateway()
            .await
            .expect("stop gateway succeeds");
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Client settings trusting the gateway's internal CA `root`, resolving
    /// each of `hosts` to loopback.
    pub(super) fn trusting(root: &reqwest::Certificate, hosts: &[&str]) -> reqwest::ClientBuilder {
        let client = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(root.clone());
        hosts.iter().fold(client, |client, host| {
            client.resolve(host, "127.0.0.1:443".parse().expect("the value parses"))
        })
    }

    /// Another manager of this installation's gateway, `enabled` or not.
    pub(super) fn ingress(&self, enabled: bool) -> Ingress {
        Ingress::new(
            enabled,
            &self.socket,
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .expect("the value is valid")
    }

    /// Another manager of this installation's gateway, with the same
    /// container spec and issuer as the scenario's.
    pub(super) fn replica(&self) -> Ingress {
        let mut ingress = self.ingress(true);
        ingress.issuer.clone_from(&self.gateway.issuer);
        ingress.extra_hosts.clone_from(&self.gateway.extra_hosts);
        ingress.private_port = self.gateway.private_port;
        ingress
    }

    /// Requests a deployment of `app` and scans until it succeeds.
    pub(super) async fn deploy(&self, app: NormalizedApplication) -> EnvironmentId {
        deploy(&self.store, &self.controller, app).await
    }

    /// Waits until `operation` succeeds, run by a controller alongside.
    pub(super) async fn succeeded(&self, operation: &str) {
        self.store
            .wait_until(async || {
                self.store
                    .operation(operation)
                    .await
                    .expect("the operation loads")
                    .state
                    == OperationState::Succeeded
            })
            .await;
    }

    /// The first application with `manifest`, as its next deployment.
    pub(super) fn first_as(
        &self,
        manifest: piqueld_core::manifest::ApplicationManifest,
    ) -> NormalizedApplication {
        manifest
            .validate()
            .expect("the manifest validates")
            .normalize(
                piqueld_core::ApplicationId::parse(self.first.as_str())
                    .expect("the application id parses"),
            )
    }

    /// Restarts the gateway's container behind the daemon's back.
    pub(super) async fn restart_gateway(&self) {
        let restart = bollard::query_parameters::RestartContainerOptionsBuilder::default()
            .t(1)
            .build();
        self.docker
            .client()
            .expect("the docker client is available")
            .restart_container(&self.gateway.name, Some(restart))
            .await
            .expect("restart container succeeds");
    }

    /// Requests `/<path>` from the plain HTTP listener with `host` as `Host`.
    pub(super) async fn plain(&self, host: &str, path: &str) -> reqwest::Response {
        self.client
            .get(format!("http://127.0.0.1:{}/{path}", self.plain_port))
            .header("Host", host)
            .send()
            .await
            .expect("the request is sent")
    }

    pub(super) fn url(&self, host: &str) -> String {
        format!("https://{host}:{}/", self.tls_port)
    }

    /// Waits until Caddy serves a certificate for `host`. Its internal CA
    /// issues one only after the deployment adding the route has loaded, so
    /// a request made right after `deploy` can fail the TLS handshake with
    /// `internal_error`. Call it before the first request to a new hostname.
    pub(super) async fn await_certificate(&self, host: &str) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Err(error) = self.client.head(self.url(host)).send().await {
                assert!(
                    error.is_connect() || error.is_timeout(),
                    "{host}: {error:?}"
                );
                // wall-clock: Caddy, a real process, issues the certificate on
                // its own and signals nothing; ask again until it serves it.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Caddy issues a certificate for {host}"));
    }

    pub(super) async fn body(&self, host: &str) -> String {
        self.client
            .get(self.url(host))
            .send()
            .await
            .expect("the request is sent")
            .text()
            .await
            .expect("the body reads")
    }

    /// Caddy's graceful reload may close a connection it accepted just before
    /// the handoff without reading the request. Clients retry such idempotent
    /// requests once; routing itself must still answer on that retry.
    pub(super) async fn body_across_reload(&self, host: &str) -> String {
        match self.client.get(self.url(host)).send().await {
            Ok(response) => response.text().await.expect("the body reads"),
            Err(error) if error.is_request() && !error.is_timeout() => self.body(host).await,
            Err(error) => panic!("request failed across reload: {error:?}"),
        }
    }
}

/// The certificate the server presented for `response`, when the client
/// recorded its TLS information.
pub(super) fn peer_certificate(response: &reqwest::Response) -> Option<Vec<u8>> {
    let info = response.extensions().get::<reqwest::tls::TlsInfo>()?;
    Some(info.peer_certificate()?.to_vec())
}

impl Ingress {
    /// The serving gateway container's ID; the container must exist.
    pub(super) async fn container_id(&self) -> serde_json::Value {
        self.container()
            .await
            .expect("the container lookup succeeds")
            .expect("the container exists")["Id"]
            .clone()
    }
}
