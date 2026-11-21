//! Real ACME orders and challenges against Caddy's bundled test CA, with short
//! certificates so renewal is exercised without public DNS or CA rate limits.
use super::scenario::{Scenario, peer_certificate};
use hyper::Method;
use serde_json::json;
use std::time::Duration;

impl Scenario {
    async fn configure_acme_test(&self) -> serde_json::Value {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        // Docker's test-only ExtraHosts resolve challenges to the gateway itself.
        let original = self.gateway.caddy.get("/config/").await.unwrap();
        let mut table = self.store.routing_table().await.unwrap();
        for host in hosts {
            let mut route = table[&self.first][0].clone();
            route.hostname = super::application("acme", host, "unused").spec().routes[0]
                .hostname
                .clone();
            table.get_mut(&self.first).unwrap().push(route);
        }
        let mut configuration = self.gateway.configuration(&table).await.unwrap();
        configuration["apps"]["http"]["servers"]["acme"] = json!({
            "listen":["127.0.0.1:9072"], "tls_connection_policies":[{}],
            "routes":[{"match":[{"host":["127.0.0.1"]}], "handle":[{"handler":"acme_server", "lifetime":300_000_000_000_u64,
                "challenges":["http-01", "tls-alpn-01"]}]}]
        });
        // Provision the directory's own TLS certificate before ACME clients use it.
        let mut directory = original.clone();
        directory["apps"]["http"]["servers"]["acme"] =
            configuration["apps"]["http"]["servers"]["acme"].clone();
        self.gateway
            .caddy
            .external(Method::POST, "/load", Some(&directory))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let certificate = self
                .directory
                .path()
                .join("ingress/data/caddy/certificates/local/127.0.0.1/127.0.0.1.crt");
            while !tokio::fs::try_exists(&certificate).await.unwrap() {
                // wall-clock: Caddy, a real process, writes the certificate on its
                // own and signals nothing; look again until the file exists.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("ACME directory has its own trusted TLS certificate");
        let policy = |host: &str, disabled: &str| {
            json!({
                "subjects":[host], "renewal_window_ratio":0.95,
                "issuers":[{"module":"acme", "ca":"https://127.0.0.1:9072/acme/local/directory",
                "trusted_roots_pem_files":["/data/caddy/pki/authorities/local/root.crt"],
                    "challenges":{disabled:{"disabled":true}}}]
            })
        };
        configuration["apps"]["tls"]["automation"] = json!({
            "renew_interval":"1s",
            "policies":[policy(hosts[0], "tls-alpn"), policy(hosts[1], "http"),
                {"issuers":[{"module":"internal"}]}]
        });
        self.gateway
            .caddy
            .external(Method::POST, "/load", Some(&configuration))
            .await
            .unwrap();
        // CertMagic's maintenance ticker is created at process startup. Restart
        // from the saved test config so its accelerated renewal interval takes effect.
        self.restart_gateway().await;
        original
    }

    async fn acme_challenges_and_renewal(&self) {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        let original = self.configure_acme_test().await;
        // The certificate the probe endpoint of `host` is served with. A new
        // client also discards TLS session tickets. Merely disabling the HTTP
        // pool can resume an old TLS session and report its old certificate.
        let served = async |host: &str| {
            let response = Scenario::trusting(&self.root, &hosts)
                .timeout(Duration::from_secs(3))
                .tls_info(true)
                .pool_max_idle_per_host(0)
                .build()
                .unwrap()
                .get(format!("{}.well-known/piqueld-ingress", self.url(host)))
                .send()
                .await?;
            let certificate = peer_certificate(&response).unwrap();
            assert_eq!(response.text().await.unwrap(), self.gateway.instance_id);
            reqwest::Result::Ok(certificate)
        };
        for host in hosts {
            let first = tokio::time::timeout(Duration::from_mins(1), async {
                loop {
                    if let Ok(certificate) = served(host).await {
                        break certificate;
                    }
                    // wall-clock: the ACME challenge runs between Caddy and Pebble, real
                    // processes that signal nothing; ask again until it is served.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await
            .expect("ACME challenge produces a trusted certificate");
            // No daemon reconciliation/reload occurs here: Caddy must renew by
            // itself, and a fresh TLS handshake must serve the renewed certificate.
            tokio::time::timeout(Duration::from_secs(90), async {
                loop {
                    if served(host).await.unwrap() != first {
                        break;
                    }
                    // wall-clock: Caddy renews on its own one-second ticker and signals
                    // nothing; ask again until the renewed certificate is served.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
            .await
            .expect("ACME certificate renews and is served without daemon intervention");
        }
        self.gateway
            .caddy
            .external(Method::POST, "/load", Some(&original))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_acme_challenges_and_renewal() {
    Scenario::run(Scenario::acme_challenges_and_renewal, async |_| {}).await;
}
