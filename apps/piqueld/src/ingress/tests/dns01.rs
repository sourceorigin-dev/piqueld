//! DNS-01 issuance against Pebble, with TXT records served by `challtestsrv`.
//! Both run inside the isolated Docker daemon; the harness publishes their ports.
//! The certificates serve a private route on the private listener, which this
//! test reaches through a relay adding a PROXY header, as the apps node does.
use super::{
    port, private_config,
    scenario::{Scenario, peer_certificate},
};
use crate::ingress::{Ingress, certificates::Certificates, node::proxy_relay, records::RouteDns};
use futures_util::TryStreamExt as _;
use hickory_resolver::{
    Resolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
};
use piqueld_config::AcmeConfig;
use piqueld_config::dns::{
    Dns,
    provider::{DnsProvider, Record, challtestsrv::Challtestsrv},
};
use piqueld_core::{
    api::DnsRecordState,
    event::Event,
    manifest::{Hostname, RouteHostname},
    observability::EventFilter,
};
use piqueld_store::ingress::AcmeChallenge;
use serde_json::json;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const PEBBLE_IMAGE: &str = "ghcr.io/letsencrypt/pebble:2.10.1@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199";
pub(super) const CURL_IMAGE: &str =
    "alpine/curl:8.22.0@sha256:3f21f10cf24835f7baae20931f18640cc30915ea4151363fe4d8fb59ee296dfb";
const CHALLTESTSRV_IMAGE: &str = "ghcr.io/letsencrypt/pebble-challtestsrv:2.10.1@sha256:12ce21884def456bcf9786542113949e1f19dc7738d2c70e156c2d0c38a1405b";

fn hosts(names: &[&str]) -> BTreeSet<Hostname> {
    names
        .iter()
        .map(|name| Hostname::parse(*name).unwrap())
        .collect()
}

/// The provider writing into `challtestsrv`, which serves `example.test`.
fn challtestsrv() -> Challtestsrv {
    Challtestsrv {
        management: format!("http://127.0.0.1:{}", port("PIQUELD_CHALLTESTSRV_API_PORT")),
        zones: hosts(&["example.test"]).into_iter().collect(),
    }
}

/// `challtestsrv`'s DNS server, the zone's only nameserver.
fn challtestsrv_dns() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port("PIQUELD_CHALLTESTSRV_DNS_PORT")))
}

fn routes(names: &[&str]) -> BTreeSet<RouteHostname> {
    names
        .iter()
        .map(|name| RouteHostname::parse(*name).unwrap())
        .collect()
}

impl Scenario {
    /// Pulls an image into the isolated daemon.
    pub(super) async fn pull(&self, image: &str) {
        self.gateway.external().pull_image(image).await.unwrap();
    }

    /// Starts `challtestsrv` and Pebble on a shared bridge network and returns
    /// Pebble's test root CA, which signs its ACME API certificate.
    async fn start_pebble(&self) -> Vec<u8> {
        let docker = self.gateway.external();
        docker
            .create_bridge_network("pebble", &json!({}))
            .await
            .unwrap();
        for (name, image, command, ports, env) in [
            (
                "challtestsrv",
                CHALLTESTSRV_IMAGE,
                json!([
                    "-dnsserver",
                    ":8053",
                    "-management",
                    ":8055",
                    "-http01",
                    "",
                    "-https01",
                    "",
                    "-tlsalpn01",
                    "",
                    "-doh",
                    ""
                ]),
                json!({"8053/tcp":[{"HostPort":"8053"}],"8055/tcp":[{"HostPort":"8055"}]}),
                json!([]),
            ),
            (
                "pebble",
                PEBBLE_IMAGE,
                json!([
                    "-config",
                    "test/config/pebble-config.json",
                    "-dnsserver",
                    "challtestsrv:8053"
                ]),
                json!({"14000/tcp":[{"HostPort":"14000"}]}),
                json!(["PEBBLE_VA_NOSLEEP=1", "PEBBLE_WFE_NONCEREJECT=0"]),
            ),
        ] {
            self.pull(image).await;
            let spec = json!({
                "Image":image,"Cmd":command,"Env":env,
                "HostConfig":{"NetworkMode":"pebble","PortBindings":ports}
            });
            docker.create_container(name, &spec).await.unwrap();
            docker.start_container(name).await.unwrap();
        }
        let path = bollard::query_parameters::DownloadFromContainerOptionsBuilder::default()
            .path("/test/certs/pebble.minica.pem")
            .build();
        let archive = self
            .docker
            .client()
            .unwrap()
            .download_from_container("pebble", Some(path))
            .map_ok(Vec::from)
            .try_concat()
            .await
            .unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&archive[124..136])
                .unwrap()
                .trim_matches(['\0', ' ']),
            8,
        )
        .unwrap();
        archive[512..512 + size].to_vec()
    }

    /// Removes the Pebble containers and network.
    async fn stop_pebble(&self) {
        let docker = self.gateway.external();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup of a container or network that may not exist"
        )]
        let _ = docker.remove_container("pebble").await;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup of a container or network that may not exist"
        )]
        let _ = docker.remove_container("challtestsrv").await;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup of a container or network that may not exist"
        )]
        let _ = docker.remove_network("pebble").await;
    }

    /// A second manager of this scenario's gateway, issuing from Pebble.
    fn pebble_ingress(&self, root: &std::path::Path) -> Ingress {
        let mut dns = Dns::new(vec![DnsProvider::Challtestsrv(challtestsrv()).into()]).unwrap();
        dns.nameservers = Some(vec![vec![
            SocketAddr::from(([127, 0, 0, 1], 1)),
            challtestsrv_dns(),
        ]]);
        let mut ingress = self.replica().with_private(&private_config());
        ingress.certificates = Certificates::new(
            dns,
            AcmeConfig {
                directory: format!("https://127.0.0.1:{}/dir", port("PIQUELD_PEBBLE_PORT")),
                email: Some("admin@example.test".into()),
            },
            ingress.directory.clone(),
        );
        ingress.certificates.acme_root = Some(root.to_owned());
        ingress.certificates.propagation_timeout = Duration::from_secs(3);
        ingress
    }

    /// TXT values `challtestsrv` serves at `name`.
    async fn txt(&self, name: &str) -> Vec<String> {
        let mut connection = ConnectionConfig::tcp();
        connection.port = port("PIQUELD_CHALLTESTSRV_DNS_PORT");
        let resolver = Resolver::builder_with_config(
            ResolverConfig::from_parts(
                None,
                Vec::new(),
                vec![NameServerConfig::new(
                    [127, 0, 0, 1].into(),
                    true,
                    vec![connection],
                )],
            ),
            TokioRuntimeProvider::default(),
        )
        .build()
        .unwrap();
        match resolver.txt_lookup(format!("{name}.")).await {
            Ok(lookup) => lookup
                .answers()
                .iter()
                .map(|record| record.data.to_string())
                .collect(),
            Err(error) if error.is_no_records_found() => Vec::new(),
            Err(error) => panic!("query challtestsrv: {error}"),
        }
    }

    /// Requests `https://<hostname>/` from the listener on loopback `port`
    /// without verifying its certificate.
    async fn untrusted(&self, hostname: &str, port: u16) -> reqwest::Result<reqwest::Response> {
        reqwest::Client::builder()
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .tls_info(true)
            .resolve(hostname, SocketAddr::from(([127, 0, 0, 1], port)))
            .build()
            .unwrap()
            .get(format!("https://{hostname}:{port}/"))
            .send()
            .await
    }

    /// A loopback port relaying to the private listener with a PROXY header
    /// naming a tailnet client, like the apps node.
    async fn relay(&self) -> (u16, tokio::task::JoinSet<()>) {
        let private = SocketAddr::from(([127, 0, 0, 1], self.gateway.private_port.unwrap()));
        let (relay, task) = proxy_relay(private, "100.64.0.9:41000".parse().unwrap())
            .await
            .unwrap();
        (relay.port(), task)
    }

    /// The certificate the private listener presents for `hostname`.
    async fn served_certificate(&self, hostname: &str) -> Vec<u8> {
        let (relay, _relay) = self.relay().await;
        let response = self.untrusted(hostname, relay).await.unwrap();
        peer_certificate(&response).unwrap()
    }

    /// Runs `curl` on `network` and returns its exit code and output.
    pub(super) async fn curl(&self, network: &str, arguments: &[String]) -> (i64, String) {
        let docker = self.gateway.external();
        let name = "piqueld-test-curl";
        let spec = json!({"Image":CURL_IMAGE,"Cmd":arguments,"HostConfig":{"NetworkMode":network}});
        docker.create_container(name, &spec).await.unwrap();
        docker.start_container(name).await.unwrap();
        let exited = docker.wait_container(name).await.unwrap();
        let output = self
            .gateway
            .docker
            .log_lines(name, 100, None)
            .await
            .unwrap()
            .join("\n");
        docker.remove_container(name).await.unwrap();
        (exited.unwrap(), output)
    }

    /// Applications reach the gateway's private listener over their ingress
    /// network, but complete TLS for a private route neither directly nor
    /// with a forged PROXY header. Public routes stay reachable as a control.
    async fn applications_cannot_reach_private_routes(&self, admin: &piqueld_core::EnvironmentId) {
        let network = piqueld_core::DockerNetworkName::for_ingress(admin).to_string();
        let gateway = self.gateway.container().await.unwrap().unwrap();
        let address = gateway["NetworkSettings"]["Networks"][&network]["IPAddress"]
            .as_str()
            .unwrap()
            .to_owned();
        self.pull(CURL_IMAGE).await;
        let request = |host: &str, port: u16| {
            vec![
                "-sk".to_owned(),
                "--max-time".into(),
                "5".into(),
                "--resolve".into(),
                format!("{host}:{port}:{address}"),
                format!("https://{host}:{port}/"),
            ]
        };
        assert_eq!(
            self.curl(&network, &request("one.example.test", 443))
                .await
                .0,
            0
        );
        assert_ne!(
            self.curl(&network, &request("admin.example.test", 8443))
                .await
                .0,
            0
        );
        let mut forged = request("admin.example.test", 8443);
        forged.extend(["--haproxy-protocol", "--haproxy-clientip", "100.64.0.9"].map(String::from));
        assert_ne!(self.curl(&network, &forged).await.0, 0);
    }

    /// Deploys `admin.example.test` publicly, then makes it private: the
    /// change withdraws it from the public listener.
    async fn deploy_private_route(&self) -> piqueld_core::EnvironmentId {
        let admin = |visibility: &str| {
            let mut manifest = super::application(
                "admin",
                "admin.example.test",
                "{http.request.header.X-Forwarded-For}",
            )
            .to_manifest();
            manifest.spec.routes[0].visibility = visibility.parse().unwrap();
            manifest
                .validate()
                .unwrap()
                .normalize(piqueld_core::ApplicationId::parse("admin-input").unwrap())
        };
        let id = self.deploy(admin("public")).await;
        assert_eq!(self.plain("admin.example.test", "").await.status(), 308);
        let deployed =
            admin("private").with_id(piqueld_core::ApplicationId::parse(id.as_str()).unwrap());
        self.deploy(deployed).await;
        assert_eq!(self.plain("admin.example.test", "").await.status(), 404);
        self.untrusted("admin.example.test", self.tls_port)
            .await
            .expect_err("`self.untrusted` should fail the handshake: no certificate");
        id
    }

    /// The private listener serves only private routes, with their DNS-01
    /// certificate, and backends see the PROXY header's client address.
    async fn private_listener_serves_private_routes(&self, ingress: &Ingress) {
        let private = self.gateway.private_port.unwrap();
        assert!(
            self.untrusted("one.example.test", private).await.is_err(),
            "a public route completed TLS on the private listener"
        );
        let (relay, _relay) = self.relay().await;
        let body = self
            .untrusted("admin.example.test", relay)
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "100.64.0.9");
        let node = ingress
            .named_container(&ingress.node_name())
            .await
            .unwrap()
            .expect("the apps node runs");
        assert_eq!(node["HostConfig"]["ReadonlyRootfs"], true);
        tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                ingress.synchronize().await.unwrap();
                if !ingress.status().await.private.state.is_empty() {
                    break;
                }
                // wall-clock: the apps node is a real container that reports its
                // state on its own; synchronize and look again.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("the apps node reports its state");
    }

    /// The newest 100 `kind` events, filtered to journal `phase`.
    async fn actions(&self, kind: &str, phase: &str) -> Vec<Event> {
        let filter = EventFilter {
            kind: Some(kind.into()),
            descending: true,
            ..EventFilter::default()
        };
        let mut events = self
            .store
            .filtered_events(&filter, &piqueld_store::Visibility::ALL, None, 100)
            .await
            .unwrap()
            .items;
        events.retain(|e| e.phase.map(piqueld_core::Phase::as_str) == Some(phase));
        events
    }

    /// The leaf certificate stored in file `name`, after its private key.
    async fn stored_leaf(&self, name: &str) -> Vec<u8> {
        let bundle = tokio::fs::read_to_string(
            self.directory
                .path()
                .join("ingress/certificates")
                .join(name),
        )
        .await
        .unwrap();
        let chain = &bundle[bundle.find("-----BEGIN CERTIFICATE-----").unwrap()..];
        x509_parser::pem::parse_x509_pem(chain.as_bytes())
            .unwrap()
            .1
            .contents
    }

    async fn dns01_certificates(&self) {
        let root = self.start_pebble().await;
        let root_path = self.directory.path().join("pebble.minica.pem");
        tokio::fs::write(&root_path, &root).await.unwrap();
        let ingress = self.pebble_ingress(&root_path);
        Self::route_checks_resolve_authoritatively(&ingress).await;
        let admin = self.deploy_private_route().await;
        let desired = routes(&["admin.example.test", "www.example.test", "example.test"]);
        let now = piqueld_store::now_ms();
        let expires = self.dns01_issue(&ingress, &desired, now).await;
        self.private_listener_serves_private_routes(&ingress).await;
        self.applications_cannot_reach_private_routes(&admin).await;
        self.dns01_renew(&ingress, &desired, expires).await;
        self.dns01_failure(&ingress, now).await;
        self.dns01_cancel(&root_path, now).await;
        self.dns01_leftover(&ingress, now).await;
        self.dns01_retire(&ingress, now, expires).await;
        self.wildcard_routes(&ingress, now).await;
        ingress.synchronize().await.unwrap();
    }

    /// A public wildcard route is served with its own DNS-01 certificate.
    /// Names one label below its parent reach its service with their own
    /// `Host`, an exact route of another application under the same parent
    /// still wins, and a name two labels deep is not matched.
    async fn wildcard_routes(&self, ingress: &Ingress, now: i64) {
        let tenants = super::application(
            "tenants",
            "*.tenants.example.test",
            "{http.request.host} {http.request.header.X-Forwarded-Host}",
        );
        let exact = super::application("exact", "exact.tenants.example.test", "exact");
        self.deploy(exact).await;
        self.deploy(tenants).await;
        let desired = routes(&["*.tenants.example.test"]);
        let table = self.store.routing_table().await.unwrap();
        assert!(ingress.dns01_hostnames(&table).is_superset(&desired));
        ingress
            .maintain_certificates(&desired, now, &CancellationToken::new())
            .await;
        ingress.synchronize().await.unwrap();

        let answer = |host: &'static str| async move {
            let response = self.untrusted(host, self.tls_port).await.ok()?;
            let certificate = peer_certificate(&response)?;
            Some((certificate, response.text().await.ok()?))
        };
        let wildcard = self.stored_leaf("_wildcard.tenants.example.test.pem").await;
        for host in ["alice.tenants.example.test", "bob.tenants.example.test"] {
            let body = format!("{host} {host}:{}", self.tls_port);
            assert_eq!(answer(host).await, Some((wildcard.clone(), body)));
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match answer("exact.tenants.example.test").await {
                    Some((certificate, body)) if certificate != wildcard => {
                        break assert_eq!(body, "exact");
                    }
                    // wall-clock: Caddy, a real process, issues the certificate on its
                    // own and signals nothing; ask again until it serves it.
                    _ => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        })
        .await
        .expect("the exact route gets its own certificate");

        self.untrusted("a.b.tenants.example.test", self.tls_port)
            .await
            .expect_err("`self.untrusted` should fail the handshake: no certificate");
        assert_eq!(
            self.plain("alice.tenants.example.test", "").await.status(),
            308
        );
        assert_eq!(
            self.plain("a.b.tenants.example.test", "").await.status(),
            404
        );
    }

    /// Two children share the zone's wildcard and the apex gets an exact name;
    /// the private listener serves them. Returns the latest expiry.
    async fn dns01_issue(
        &self,
        ingress: &Ingress,
        desired: &BTreeSet<RouteHostname>,
        now: i64,
    ) -> i64 {
        use std::os::unix::fs::PermissionsExt;
        tokio::time::timeout(
            Duration::from_mins(3),
            ingress.maintain_certificates(desired, now, &CancellationToken::new()),
        )
        .await
        .expect("issuance finishes");
        let status = ingress.dns_status().await;
        assert_eq!(status.providers[0].zones, ["example.test"]);
        let names: Vec<_> = status
            .certificates
            .iter()
            .map(|c| (c.name.as_str(), c.hostnames.len(), c.error.as_deref()))
            .collect();
        assert_eq!(
            names,
            [("*.example.test", 2, None), ("example.test", 1, None)]
        );
        assert_eq!(
            self.txt("_acme-challenge.example.test").await,
            [] as [String; 0]
        );
        for file in ["_wildcard.example.test.pem", "example.test.pem"] {
            let path = self
                .directory
                .path()
                .join("ingress/certificates")
                .join(file);
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        ingress.synchronize().await.unwrap();
        assert_eq!(
            self.served_certificate("admin.example.test").await,
            self.stored_leaf("_wildcard.example.test.pem").await
        );
        status
            .certificates
            .iter()
            .map(|c| c.expires_at_ms.unwrap())
            .max()
            .unwrap()
    }

    /// With a third of the lifetime left both are renewed, and nothing else.
    async fn dns01_renew(
        &self,
        ingress: &Ingress,
        desired: &BTreeSet<RouteHostname>,
        expires: i64,
    ) {
        let stored = self.stored_leaf("_wildcard.example.test.pem").await;
        ingress
            .maintain_certificates(desired, expires - 86_400_000, &CancellationToken::new())
            .await;
        assert_eq!(
            self.actions("action_succeeded", "ingress_certificate_renew")
                .await
                .len(),
            2
        );
        assert_ne!(self.stored_leaf("_wildcard.example.test.pem").await, stored);
    }

    /// A record that never becomes visible fails issuance after it was
    /// created: it is still deleted, the error is reported, and the next
    /// attempt waits for the backoff.
    async fn dns01_failure(&self, ingress: &Ingress, now: i64) {
        let DnsProvider::Challtestsrv(provider) = ingress.certificates.dns.provider(0) else {
            panic!("the test configures the Challtestsrv DNS provider")
        };
        let http = ingress.certificates.dns.http();
        provider
            .servfail(http, "_acme-challenge.fail.example.test", true)
            .await;
        let failing = routes(&["x.fail.example.test"]);
        ingress
            .maintain_certificates(&failing, now, &CancellationToken::new())
            .await;
        provider
            .servfail(http, "_acme-challenge.fail.example.test", false)
            .await;
        let status = ingress.dns_status().await;
        let failed = status
            .certificates
            .iter()
            .find(|c| c.name == "*.fail.example.test")
            .unwrap();
        assert!(
            failed.error.as_deref().unwrap().contains("was not visible"),
            "{failed:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.fail.example.test").await,
            [] as [String; 0]
        );
        assert!(
            self.actions("action_failed", "ingress_certificate_issue")
                .await
                .iter()
                .any(|e| e.error_code.as_deref() == Some("certificate_renewal_failed"))
        );
        let attempts = self
            .actions("action_started", "ingress_certificate_issue")
            .await
            .len();
        ingress
            .maintain_certificates(&failing, now + 1000, &CancellationToken::new())
            .await;
        assert_eq!(
            self.actions("action_started", "ingress_certificate_issue")
                .await
                .len(),
            attempts
        );
    }

    /// Shutdown while the order waits for propagation, after the TXT record
    /// was created, interrupts it and the record is still deleted. This
    /// manager's only nameserver refuses connections, so the record never
    /// propagates and the wait lasts until shutdown.
    async fn dns01_cancel(&self, root: &std::path::Path, now: i64) {
        let mut ingress = self.pebble_ingress(root);
        ingress.certificates.dns.nameservers =
            Some(vec![vec![SocketAddr::from(([127, 0, 0, 1], 1))]]);
        let shutdown = CancellationToken::new();
        let cancelled = routes(&["x.cancel.example.test"]);
        tokio::join!(
            ingress.maintain_certificates(&cancelled, now, &shutdown),
            async {
                while self
                    .txt("_acme-challenge.cancel.example.test")
                    .await
                    .is_empty()
                {
                    // wall-clock: the TXT record shows up in the real test DNS server,
                    // which signals nothing; resolve it again until it does.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                shutdown.cancel();
            }
        );
        let status = ingress.dns_status().await;
        let interrupted = status
            .certificates
            .iter()
            .find(|c| c.name == "*.cancel.example.test")
            .unwrap();
        assert!(
            interrupted
                .error
                .as_deref()
                .unwrap()
                .contains("cancelled by daemon shutdown"),
            "{interrupted:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.cancel.example.test").await,
            [] as [String; 0]
        );
    }

    /// A saved challenge record whose deletion failed earlier, or was cut
    /// short by a restart, is deleted at the start of the next pass, before
    /// its order creates a record of its own. `challtestsrv` clears every
    /// value at a name, so deleting it afterwards would also remove the new
    /// record and fail validation. The successful order then forgets its own
    /// record too.
    async fn dns01_leftover(&self, ingress: &Ingress, now: i64) {
        let dns = &ingress.certificates.dns;
        let hostname = Hostname::parse("x.leftover.example.test").unwrap();
        let (provider, zone) = dns.zone_for(&hostname).await.unwrap();
        let id = dns
            .provider(provider)
            .upsert(
                dns.http(),
                &zone,
                "_acme-challenge.leftover.example.test",
                None,
                &Record::Txt("stale".into()),
            )
            .await
            .unwrap();
        let leftover = AcmeChallenge {
            name: "_acme-challenge.leftover.example.test".into(),
            zone: zone.name,
            value: "stale".into(),
            record_id: Some(id.0),
        };
        ingress.store.save_acme_challenge(&leftover).await.unwrap();
        ingress
            .maintain_certificates(&[hostname.into()].into(), now, &CancellationToken::new())
            .await;
        let status = ingress.dns_status().await;
        let issued = status
            .certificates
            .iter()
            .find(|c| c.name == "*.leftover.example.test")
            .unwrap();
        assert!(
            issued.error.is_none() && issued.expires_at_ms.is_some(),
            "{issued:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.leftover.example.test").await,
            [] as [String; 0]
        );
        assert_eq!(ingress.store.acme_challenges().await.unwrap(), []);
    }

    /// A route check resolves a hostname whose records piqueld manages on its
    /// zone's nameserver, `challtestsrv`, so it finds records the host's
    /// resolver has no answer for, as when it cached NXDOMAIN before piqueld
    /// published them. Records the operator writes go through that resolver.
    async fn route_checks_resolve_authoritatively(ingress: &Ingress) {
        let dns = &ingress.certificates.dns;
        dns.discover().await;
        let addresses: [IpAddr; 2] = ["192.0.2.7".parse().unwrap(), "2001:db8::7".parse().unwrap()];
        challtestsrv()
            .answer(dns.http(), "fresh.example.test", &addresses)
            .await;
        let route = RouteHostname::parse("fresh.example.test").unwrap();
        let hostname = Hostname::parse("fresh.example.test").unwrap();
        assert!(
            ingress.resolve(&route, &hostname).await.is_err(),
            "the host's resolver must have no answer"
        );
        ingress.records.write().insert(
            route.clone(),
            RouteDns {
                state: DnsRecordState::Managed,
                message: None,
            },
        );
        assert_eq!(
            ingress.resolve(&route, &hostname).await.unwrap(),
            BTreeSet::from(addresses)
        );
    }

    /// Unneeded certificates are kept until they expire, then deleted.
    async fn dns01_retire(&self, ingress: &Ingress, now: i64, expires: i64) {
        ingress
            .maintain_certificates(&BTreeSet::new(), now, &CancellationToken::new())
            .await;
        assert_eq!(ingress.dns_status().await.certificates.len(), 3);
        ingress
            .maintain_certificates(
                &BTreeSet::new(),
                expires + 365 * 86_400_000,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(ingress.dns_status().await.certificates, []);
        assert!(
            !self
                .directory
                .path()
                .join("ingress/certificates/example.test.pem")
                .exists()
        );
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_dns01_certificates() {
    Scenario::run(Scenario::dns01_certificates, Scenario::stop_pebble).await;
}
