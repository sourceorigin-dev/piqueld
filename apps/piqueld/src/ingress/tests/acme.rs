//! Real ACME orders and challenges against Caddy's bundled test CA, with short
//! certificates so renewal is exercised without public DNS or CA rate limits.
use super::scenario::{Scenario, peer_certificate};
use hyper::Method;
use piqueld_core::wait;
use serde_json::json;
use std::time::Duration;

impl Scenario {
    async fn configure_acme_test(&self) -> serde_json::Value {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        let original = self
            .gateway
            .caddy
            .get("/config/")
            .await
            .expect("the Caddy configuration is read");
        let mut table = self
            .store
            .routing_table()
            .await
            .expect("the routing table loads");
        for host in hosts {
            let mut route = table[&self.first][0].clone();
            route.hostname = super::application("acme", host, "unused").spec().routes[0]
                .hostname
                .clone();
            table
                .get_mut(&self.first)
                .expect("the entry exists")
                .push(route);
        }
        let mut configuration = self
            .gateway
            .configuration(&table)
            .await
            .expect("the gateway configuration is read");
        configuration["apps"]["http"]["servers"]["acme"] = json!({
            "listen":["127.0.0.1:9072"], "tls_connection_policies":[{}],
            "routes":[{"match":[{"host":["127.0.0.1"]}], "handle":[{"handler":"acme_server", "lifetime":300_000_000_000_u64,
                "challenges":["http-01", "tls-alpn-01"]}]}]
        });
        let mut directory = original.clone();
        directory["apps"]["http"]["servers"]["acme"] =
            configuration["apps"]["http"]["servers"]["acme"].clone();
        self.gateway
            .caddy
            .external(Method::POST, "/load", Some(&directory))
            .await
            .expect("the Caddy admin call succeeds");
        tokio::time::timeout(Duration::from_secs(10), async {
            let certificate = self
                .directory
                .path()
                .join("ingress/data/caddy/certificates/local/127.0.0.1/127.0.0.1.crt");
            while !tokio::fs::try_exists(&certificate)
                .await
                .expect("the path is checked")
            {
                // wall-clock: Caddy, a real process, writes the certificate on its
                // own and signals nothing; look again until the file exists.
                wait::pause(Duration::from_millis(50)).await;
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
            .expect("the Caddy admin call succeeds");
        self.restart_gateway().await;
        original
    }

    async fn acme_challenges_and_renewal(&self) {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        let original = self.configure_acme_test().await;
        let served = async |host: &str| {
            let response = Scenario::trusting(&self.root, &hosts)
                .timeout(Duration::from_secs(3))
                .tls_info(true)
                .pool_max_idle_per_host(0)
                .build()
                .expect("the client builds")
                .get(format!("{}.well-known/piqueld-ingress", self.url(host)))
                .send()
                .await?;
            let certificate =
                peer_certificate(&response).expect("the response has a peer certificate");
            assert_eq!(
                response.text().await.expect("the body reads"),
                self.gateway.instance_id
            );
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
                    wait::pause(Duration::from_millis(250)).await;
                }
            })
            .await
            .expect("ACME challenge produces a trusted certificate");
            tokio::time::timeout(Duration::from_secs(90), async {
                loop {
                    if served(host).await.expect("the host answers") != first {
                        break;
                    }
                    // wall-clock: Caddy renews on its own one-second ticker and signals
                    // nothing; ask again until the renewed certificate is served.
                    wait::pause(Duration::from_secs(1)).await;
                }
            })
            .await
            .expect("ACME certificate renews and is served without daemon intervention");
        }
        self.gateway
            .caddy
            .external(Method::POST, "/load", Some(&original))
            .await
            .expect("the Caddy admin call succeeds");
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_acme_challenges_and_renewal() {
    Scenario::run(Scenario::acme_challenges_and_renewal, async |_| {}).await;
}
