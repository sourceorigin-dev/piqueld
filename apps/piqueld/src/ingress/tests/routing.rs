//! Routing through a real gateway: TLS, redirects, network attachments,
//! backend cutovers, forwarded addresses, traffic counts, and disabling and
//! re-enabling ingress.
use super::*;

impl Scenario {
    async fn assert_public_routing(&self) {
        // Deployment journaled the gateway start; a converged gateway records nothing.
        let history = self.store.events(None, None, 100).await.unwrap().items;
        assert!(history.iter().any(|event| event.kind == "action_succeeded"
            && event.phase.map(piqueld_core::Phase::as_str) == Some("ingress_start_gateway")));
        let retained = history.len();
        self.gateway.synchronize().await.unwrap();
        assert_eq!(
            self.store
                .events(None, None, 100)
                .await
                .unwrap()
                .items
                .len(),
            retained
        );
        let container = self.gateway.container().await.unwrap().unwrap();
        assert_eq!(
            container["HostConfig"]["PortBindings"]["443/tcp"][0]["HostPort"],
            "443"
        );
        assert_eq!(
            container["NetworkSettings"]["Networks"][&self.gateway.name]["GwPriority"],
            1
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        let redirect = self.plain("one.example.test", "path?q=1").await;
        assert_eq!(redirect.status(), 308);
        assert_eq!(
            redirect.headers()["location"],
            "https://one.example.test/path?q=1"
        );
        assert_eq!(self.plain("unknown.example.test", "").await.status(), 404);
    }

    /// A service-less application's redirect is answered by Caddy itself, so
    /// the application has no ingress network.
    async fn redirect_without_backend(&self) {
        let app = piqueld_core::parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='www'\n[[spec.routes]]\nhostname='www.example.test'\nvisibility='public'\nredirect={to='https://one.example.test/base/'}").unwrap().normalize(piqueld_core::ApplicationId::parse("input-app").unwrap());
        let id = self.deploy(app).await;
        self.await_certificate("www.example.test").await;
        let response = self
            .client
            .get(format!("{}path?q=1", self.url("www.example.test")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 308);
        assert_eq!(
            response.headers()["location"],
            "https://one.example.test/base/path?q=1"
        );
        assert_eq!(self.docker.observe(&id).await.unwrap().networks, []);
    }

    async fn add_application_without_interrupting_traffic(&self) {
        let original = self.gateway.container_id().await;
        let completed = std::sync::atomic::AtomicBool::new(false);
        let (second, requests) = tokio::join!(
            async {
                let id = self
                    .deploy(application("two", "two.example.test", "second backend"))
                    .await;
                completed.store(true, std::sync::atomic::Ordering::SeqCst);
                id
            },
            async {
                let mut requests = 0;
                while !completed.load(std::sync::atomic::Ordering::SeqCst) {
                    assert_eq!(
                        self.body_across_reload("one.example.test").await,
                        "first backend"
                    );
                    requests += 1;
                    // wall-clock: paces requests through the real gateway while the
                    // deployment runs, to catch any reload that drops one.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                requests
            }
        );
        assert!(requests > 0);
        assert_eq!(
            self.gateway.container_id().await,
            original,
            "network attachment must not replace Caddy"
        );
        self.await_certificate("two.example.test").await;
        assert_eq!(self.body("two.example.test").await, "second backend");
        let one = self.docker.observe(&self.first).await.unwrap();
        let two = self.docker.observe(&second).await.unwrap();
        assert!(
            one.services[0]
                .networks
                .iter()
                .all(|network| !two.services[0].networks.contains(network)),
            "applications do not share backend networks"
        );
        assert_eq!(one.services[0].networks.len(), 2);
    }

    async fn slow_gateway_does_not_block_private_deployment(&self) {
        let guard = self.gateway.update.lock().await;
        let (_, routed) = request_deployment(
            &self.store,
            Actor::Daemon,
            application("one", "one.example.test", "first backend"),
        )
        .await;
        let cancellation = CancellationToken::new();
        tokio::join!(
            async {
                self.controller
                    .run(
                        Arc::new(tokio::sync::Notify::new()),
                        Duration::from_millis(100),
                        30,
                        30,
                        cancellation.clone(),
                    )
                    .await
                    .unwrap();
            },
            async {
                tokio::time::timeout(Duration::from_mins(1), async {
                    self.store
                        .wait_until(async || {
                            self.store
                                .operation(&routed)
                                .await
                                .unwrap()
                                .phase
                                .map(piqueld_core::Phase::as_str)
                                == Some("routing")
                        })
                        .await;
                    let mut private =
                        application("private", "private.example.test", "private backend")
                            .to_manifest();
                    private.spec.routes.clear();
                    let (_, operation) = request_deployment(
                        &self.store,
                        Actor::Daemon,
                        private.validate().unwrap().normalize(
                            piqueld_core::ApplicationId::parse("private-input").unwrap(),
                        ),
                    )
                    .await;
                    self.succeeded(&operation).await;
                    assert_eq!(
                        self.store.operation(&routed).await.unwrap().state,
                        OperationState::Running
                    );
                })
                .await
                .expect("private deployment proceeds while gateway writer is blocked");
                drop(guard);
                tokio::time::timeout(Duration::from_mins(1), self.succeeded(&routed))
                    .await
                    .unwrap();
                cancellation.cancel();
            }
        );
    }

    async fn restart_without_daemon(&self) {
        self.restart_gateway().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(response) = self.client.get(self.url("one.example.test")).send().await
                    && response.status().is_success()
                {
                    break;
                }
                // wall-clock: Caddy, a real process, comes back on its own after the
                // restart and signals nothing; ask again until it answers.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn release_unhealthy_backend(&self) {
        // Wait for a running but unhealthy replacement and prove the old
        // backend keeps serving before releasing the health check.
        let next_name = piqueld_core::DockerServiceName::for_service(
            &self.first,
            &piqueld_core::ServiceName::parse("next").unwrap(),
        );
        let container = tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                let containers = self
                    .docker
                    .client()
                    .unwrap()
                    .list_containers(None::<bollard::query_parameters::ListContainersOptions>)
                    .await
                    .unwrap();
                if let Some(id) = containers.into_iter().find_map(|container| {
                    let labels = container.labels?;
                    let service = labels.get("com.docker.swarm.service.name")?;
                    (service == next_name.as_str()).then_some(container.id?)
                }) {
                    break id;
                }
                assert_eq!(
                    self.body_across_reload("one.example.test").await,
                    "first backend"
                );
                // wall-clock: Swarm starts the replacement's container on its own; look
                // again while checking traffic stays on the first backend.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        for _ in 0..5 {
            assert_eq!(
                self.body_across_reload("one.example.test").await,
                "first backend"
            );
            assert_eq!(
                self.store.applied_routes(&self.first).await.unwrap()[0]
                    .target
                    .service()
                    .unwrap()
                    .as_str(),
                "web"
            );
            // wall-clock: the unhealthy replacement gets real time to take
            // over in Swarm, which it must not; nothing signals that it never will.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let health = self
            .gateway
            .docker
            .inspect_container(&container)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health["State"]["Running"], true);
        assert!(
            health["State"]["Health"]["Log"]
                .as_array()
                .unwrap()
                .iter()
                .any(|probe| probe["ExitCode"] == 1),
            "cutover must remain blocked after a real failed health probe"
        );
        let client = self.docker.client().unwrap();
        let exec = bollard::models::ExecConfig {
            cmd: Some(vec!["touch".into(), "/tmp/ready".into()]),
            ..Default::default()
        };
        let exec = client.create_exec(&container, exec).await.unwrap();
        let start = bollard::exec::StartExecOptions {
            detach: true,
            ..Default::default()
        };
        client.start_exec(&exec.id, Some(start)).await.unwrap();
    }

    async fn repoint_after_backend_convergence(&self) {
        let mut changed = application("one", "one.example.test", "first backend").to_manifest();
        let mut next = changed.spec.services[0].clone();
        next.name = "next".into();
        next.arguments = vec![
            "respond",
            "--listen",
            ":8080",
            "--body",
            "replacement backend",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        next.healthcheck = Some(piqueld_core::manifest::HealthCheck::Command {
            // Keep the replacement unhealthy until the test explicitly releases it.
            command: vec!["test".into(), "-f".into(), "/tmp/ready".into()],
            interval_seconds: 3.into(),
            timeout_seconds: 1.into(),
            start_period_seconds: 0.into(),
            retries: 3.into(),
        });
        changed.spec.services.push(next);
        changed.spec.routes[0].service = Some("next".into());
        let completed = std::sync::atomic::AtomicBool::new(false);
        tokio::join!(
            async {
                Box::pin(self.deploy(self.first_as(changed))).await;
                completed.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            async {
                self.release_unhealthy_backend().await;
                while !completed.load(std::sync::atomic::Ordering::SeqCst) {
                    let body = self.body_across_reload("one.example.test").await;
                    assert!(
                        matches!(body.as_str(), "first backend" | "replacement backend"),
                        "{body}"
                    );
                    // wall-clock: paces requests through the real gateway while the
                    // deployment runs, to catch any reload that drops one.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        );
        let observed = self.docker.observe(&self.first).await.unwrap();
        let web = piqueld_core::DockerServiceName::for_service(
            &self.first,
            &piqueld_core::ServiceName::parse("web").unwrap(),
        );
        assert_eq!(
            observed
                .services
                .iter()
                .find(|service| service.name == web.as_str())
                .unwrap()
                .networks
                .len(),
            1,
            "previous backend loses its ingress attachment only after cutover"
        );
        assert_eq!(
            self.store.applied_routes(&self.first).await.unwrap()[0]
                .target
                .service()
                .unwrap()
                .as_str(),
            "next"
        );
        assert_eq!(self.body("one.example.test").await, "replacement backend");
    }

    /// Backends see Caddy's forwarded client address from a peer inside the
    /// injected ingress range; a client-supplied forwarding header is discarded.
    async fn forwarded_client_addresses(&self) {
        let id = self
            .deploy(application(
                "three",
                "three.example.test",
                "{http.request.remote.host} {http.request.header.X-Forwarded-For} {env.PIQUELD_INGRESS_PROXIES}",
            ))
            .await;
        self.await_certificate("three.example.test").await;
        let body = self
            .client
            .get(self.url("three.example.test"))
            .header("X-Forwarded-For", "203.0.113.9")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let [peer, forwarded, proxies] = body.split(' ').collect::<Vec<_>>()[..] else {
            panic!("unexpected backend response: {body}");
        };
        let peer: std::net::Ipv4Addr = peer.parse().unwrap();
        forwarded.parse::<IpAddr>().unwrap();
        assert_ne!(
            forwarded, "203.0.113.9",
            "client-supplied header reached the backend"
        );
        let (network, prefix) = proxies.split_once('/').unwrap();
        let mask = u32::MAX << (32 - prefix.parse::<u32>().unwrap());
        assert_eq!(
            u32::from(peer) & mask,
            u32::from(network.parse::<std::net::Ipv4Addr>().unwrap()),
            "{peer} is outside {proxies}"
        );
        let observed = self.docker.observe(&id).await.unwrap();
        assert!(observed.services[0].runtime_configuration_matches);
        assert!(
            !observed.services[0]
                .environment
                .contains_key(piqueld_core::resource::INGRESS_PROXIES_ENV)
        );
    }

    /// The gateway logs each request a route proxies to the daemon, which
    /// counts it under the route's environment. Probes aren't counted, and
    /// bots aren't visitors.
    async fn count_routed_requests(&self) {
        let id = self
            .deploy(application(
                "four",
                "four.example.test",
                "<html>four</html>",
            ))
            .await;
        self.await_certificate("four.example.test").await;
        let url = format!("{}play?day=3", self.url("four.example.test"));
        for _ in 0..3 {
            let response = self
                .client
                .get(&url)
                .header("User-Agent", "Mozilla/5.0 Firefox/150.0")
                .header("Sec-Fetch-Dest", "document")
                .header("Referer", "https://news.example.org/item?id=1")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
        }
        let bot = self.client.get(&url).header("User-Agent", "Googlebot/2.1");
        assert!(bot.send().await.unwrap().status().is_success());
        let probe = format!(
            "{}.well-known/piqueld-ingress",
            self.url("four.example.test")
        );
        self.client.get(probe).send().await.unwrap();
        // Lines reach the daemon as Caddy writes them; stopping the counter
        // flushes them.
        // wall-clock: Caddy writes access log lines on its own schedule and
        // signals nothing; give it time before the counter stops.
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.counting.cancel();
        let counter = self.counter.lock().await.take().unwrap();
        counter.await.unwrap();
        let now = now_ms();
        let report = self
            .store
            .traffic(
                &id,
                piqueld_store::traffic::TrafficQuery {
                    since_ms: now - 3_600_000,
                    until_ms: now + 3_600_000,
                    service: None,
                    hostname: None,
                },
            )
            .await
            .unwrap();
        let totals = &report.totals;
        assert_eq!((totals.page_views, totals.visitors), (3, 1), "{report:?}");
        // Waiting for the certificate sent HEAD requests too; they count as
        // requests, never as page views.
        assert!(totals.requests >= 4, "{report:?}");
        assert_eq!(report.routes.len(), 1);
        assert_eq!(
            (
                report.routes[0].service.as_str(),
                report.routes[0].hostname.as_str()
            ),
            ("web", "four.example.test")
        );
        assert_eq!(report.pages[0].value, "/play");
        assert_eq!(report.pages[0].page_views, 3);
        assert_eq!(report.referrers[0].value, "news.example.org");
    }

    async fn disable_and_restore_deployed_routes(&self) {
        let disabled = Arc::new(self.ingress(false));
        disabled.synchronize().await.unwrap();
        assert!(disabled.container().await.unwrap().is_none());
        self.client
            .get(self.url("one.example.test"))
            .send()
            .await
            .expect_err("`` should be rejected");
        assert!(
            self.directory
                .path()
                .join(scenario::ROOT_CERTIFICATE)
                .exists()
        );
        let sources = Arc::new(Sources::open(self.directory.path()).await.unwrap());
        let controller =
            Controller::new(Arc::clone(&self.docker), Arc::clone(&self.store), sources)
                .with_ingress(Arc::clone(&disabled));
        let mut changed =
            application("one", "one.example.test", "updated while disabled").to_manifest();
        changed.spec.routes.clear();
        deploy(&self.store, &controller, self.first_as(changed)).await;
        assert_eq!(
            self.store.routing_table().await.unwrap()[&self.first],
            [] as [ValidatedRoute; 0]
        );
        assert_eq!(
            self.docker.observe(&self.first).await.unwrap().services[0]
                .networks
                .len(),
            1
        );
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                self.controller
                    .scan(&CancellationToken::new())
                    .await
                    .unwrap();
                // Re-enable may recreate Swarm endpoints; route application and
                // public backend reachability are deliberately separate states.
                if self.gateway.synchronize().await.is_ok()
                    && let Ok(response) = self.client.get(self.url("two.example.test")).send().await
                    && response.status().is_success()
                    && response
                        .text()
                        .await
                        .is_ok_and(|body| body == "second backend")
                {
                    break;
                }
                // wall-clock: Swarm recreates the endpoints on its own and signals
                // nothing; synchronize and ask again until the backend answers.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.plain("one.example.test", "").await.status(), 404);
        assert_eq!(self.body("two.example.test").await, "second backend");
        disabled.synchronize().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_routes_tls_network_changes_and_disable() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .unwrap();
    let scenario = Box::pin(Scenario::new()).await;
    scenario.assert_public_routing().await;
    scenario.redirect_without_backend().await;
    scenario
        .slow_gateway_does_not_block_private_deployment()
        .await;
    scenario.persistent_traffic_survives_reload().await;
    scenario.replacement_preserves_unverified_attachment().await;
    scenario.replacement_failure_and_recovery().await;
    scenario
        .committed_replacement_survives_stale_cleanup()
        .await;
    Box::pin(scenario.unavailable_network_does_not_block_withdrawals()).await;
    Box::pin(scenario.add_application_without_interrupting_traffic()).await;
    scenario.restart_without_daemon().await;
    Box::pin(scenario.repoint_after_backend_convergence()).await;
    scenario.forwarded_client_addresses().await;
    scenario.count_routed_requests().await;
    scenario.disable_and_restore_deployed_routes().await;
}
