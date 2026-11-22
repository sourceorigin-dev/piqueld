//! The Caddy configuration and apps node spec built for each listener and
//! the daemon's own hostnames.
use super::*;

#[tokio::test]
async fn each_listener_serves_only_its_own_routes() {
    let (_engine, ingress) = private_ingress().await;
    let table = listener_table();
    let hosts = |server: &serde_json::Value| {
        server["routes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|route| route["match"][0]["host"][0].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>()
    };
    let edge = edge();
    let configuration = ingress.build_configuration(
        &table,
        &ingress.exact_hostnames(&table).await.unwrap(),
        Some(&edge),
        None,
    );
    let servers = &configuration["apps"]["http"]["servers"];
    for (https, http, host) in [
        ("public", "public_http", "www.example.com"),
        ("private", "private_http", "admin.example.com"),
    ] {
        assert_eq!(hosts(&servers[https]), [host.to_owned()].into(), "{https}");
        assert_eq!(hosts(&servers[http]), [host.to_owned()].into(), "{http}");
        // No other hostname completes TLS on this listener.
        assert_eq!(
            servers[https]["tls_connection_policies"][0]["match"]["sni"],
            json!([host])
        );
    }
    // Applications reach the private listener over their networks, so only
    // tailnet client addresses, set by edge peers' PROXY headers, complete TLS.
    let tailnet = json!({"ranges":["100.64.0.0/10", "fd7a:115c:a1e0::/48"]});
    assert_eq!(
        servers["private"]["tls_connection_policies"][0]["match"]["remote_ip"],
        tailnet
    );
    assert_eq!(
        servers["private_http"]["routes"][0]["match"],
        json!([{"not":[{"remote_ip":tailnet}]}])
    );
    assert!(
        servers["public"]["tls_connection_policies"][0]["match"]
            .get("remote_ip")
            .is_none()
    );
    assert_eq!(servers["public"]["listen"], json!([":443"]));
    assert_eq!(servers["private"]["listen"], json!([":8443"]));
    let wrapper = json!({"wrapper":"proxy_protocol","allow":edge,"fallback_policy":"ignore"});
    assert_eq!(
        servers["private"]["listener_wrappers"],
        json!([wrapper, {"wrapper":"tls"}])
    );
    assert_eq!(
        servers["private_http"]["listener_wrappers"],
        json!([wrapper])
    );
    // Private certificates come from DNS-01 only.
    assert_eq!(
        servers["private"]["automatic_https"],
        json!({"disable":true})
    );

    // Without private ingress, private routes are served nowhere, never on
    // the public listener.
    let configuration = ingress.build_configuration(
        &table,
        &ingress.exact_hostnames(&table).await.unwrap(),
        None,
        None,
    );
    let servers = configuration["apps"]["http"]["servers"]
        .as_object()
        .unwrap();
    assert_eq!(
        servers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["public", "public_http"]
    );
    assert_eq!(
        hosts(&servers["public"]),
        ["www.example.com".to_owned()].into()
    );
}

#[tokio::test]
async fn exact_hostnames_win_over_wildcards_covering_them() {
    let (engine, ingress) = private_ingress().await;
    let hooks = Hostname::parse("hooks.example.com").unwrap();
    // The website is reserved without being served by the gateway.
    let website = Hostname::parse("piqueld.example.com").unwrap();
    let reserved = [hooks.clone(), website];
    engine
        .store
        .reserve_installation_hostnames(&reserved)
        .await
        .unwrap();
    let ingress = ingress.with_webhooks(Some(hooks));
    let table: RoutingTable = [
        (
            "env-deeper",
            routes(Visibility::Public, "api.eu.example.com"),
        ),
        ("env-exact", routes(Visibility::Public, "www.example.com")),
        (
            "env-private",
            routes(Visibility::Private, "admin.example.com"),
        ),
        ("env-wildcard", routes(Visibility::Public, "*.example.com")),
    ]
    .map(|(id, routes)| (EnvironmentId::parse(id).unwrap(), routes))
    .into();
    let edge = edge();
    let configuration = ingress.build_configuration(
        &table,
        &ingress.exact_hostnames(&table).await.unwrap(),
        Some(&edge),
        None,
    );
    let servers = &configuration["apps"]["http"]["servers"];
    let matchers = |server: &str, host: &str| {
        servers[server]["routes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|route| route["match"][0]["host"][0] == host)
            .map(|route| route["match"][0].clone())
            .collect::<Vec<_>>()
    };
    // The wildcard's probe, proxy and redirect leave out every exact
    // hostname it covers, even one served on another listener or not by
    // the gateway at all, but not deeper ones.
    let excluded = json!([{"host":[
        "hooks.example.com", "piqueld.example.com", "www.example.com", "admin.example.com"
    ]}]);
    let wildcard = matchers("public", "*.example.com");
    assert_eq!(wildcard.len(), 2);
    for matcher in wildcard
        .iter()
        .chain(&matchers("public_http", "*.example.com"))
    {
        assert_eq!(matcher["not"], excluded, "{matcher}");
    }
    assert!(
        matchers("public", "www.example.com")
            .iter()
            .all(|matcher| matcher.get("not").is_none())
    );
    // Caddy obtains no certificate for it: DNS-01 does.
    let public = &servers["public"];
    assert_eq!(
        public["tls_connection_policies"][0]["match"]["sni"],
        json!([
            "api.eu.example.com",
            "www.example.com",
            "*.example.com",
            "hooks.example.com"
        ])
    );
    // Caddy manages exact names a loaded wildcard certificate covers.
    assert_eq!(
        public["automatic_https"],
        json!({
            "disable_redirects":true,"skip_certificates":["*.example.com"],
            "ignore_loaded_certificates":true
        })
    );
    let wildcard = RouteHostname::parse("*.example.com").unwrap();
    assert!(ingress.dns01_hostnames(&table).contains(&wildcard));
    // Through a tunnel, Cloudflare terminates its TLS.
    let (_engine, tunneled) = private_ingress().await;
    let tunneled = tunneled.with_tunnel(&tunnel::tunnel("c2VjcmV0"));
    assert!(!tunneled.dns01_hostnames(&table).contains(&wildcard));
}

#[tokio::test]
async fn the_apps_node_forwards_to_the_private_listener_without_privileges() {
    let (_engine, ingress) = private_ingress().await;
    let gateway = &ingress.name;
    assert_eq!(
        ingress.serve_configuration(),
        json!({"TCP":{
            "443":{"TCPForward":format!("{gateway}:8443"),"ProxyProtocol":2},
            "80":{"TCPForward":format!("{gateway}:8081"),"ProxyProtocol":2}
        }})
    );
    let spec = ingress.node_spec(ingress.node.as_ref().unwrap());
    assert_eq!(spec["HostConfig"]["NetworkMode"], json!(gateway));
    assert_eq!(spec["HostConfig"]["CapDrop"], json!(["ALL"]));
    assert_eq!(spec["HostConfig"]["ReadonlyRootfs"], true);
    assert!(spec["HostConfig"].get("PortBindings").is_none());
    let environment = spec["Env"].as_array().unwrap();
    assert!(environment.contains(&json!("TS_USERSPACE=true")));
    assert!(environment.contains(&json!("TS_HOSTNAME=piqueld-apps")));
    // Without an auth key, the node logs in interactively.
    assert!(
        !environment
            .iter()
            .any(|value| value.as_str().unwrap().starts_with("TS_AUTHKEY"))
    );
    // A replaced auth key replaces the container, which reads it again.
    let hash = |key: &str| {
        let config = PrivateIngressConfig {
            enabled: true,
            auth_key: Some(key.into()),
            ..Default::default()
        };
        let node = node::Node::new(&config, &ingress.directory).unwrap();
        ingress.node_spec(&node)["Labels"]["io.piqueld.ingress-configuration"].clone()
    };
    assert_ne!(hash("tskey-auth-old"), hash("tskey-auth-new"));
    assert_ne!(
        hash("tskey-auth-old"),
        spec["Labels"]["io.piqueld.ingress-configuration"]
    );
}

#[tokio::test]
async fn counted_routes_tag_their_requests_and_log_them_to_the_daemon() {
    let (engine, ingress) = private_ingress().await;
    let table = listener_table();
    let edge = edge();
    let build = |ingress: &Ingress| ingress.build_configuration(&table, &[], Some(&edge), None);

    // Without counting, nothing is logged or tagged.
    let configuration = build(&ingress);
    assert!(configuration.get("logging").is_none());
    let servers = &configuration["apps"]["http"]["servers"];
    assert!(servers["public"].get("logs").is_none());
    assert!(ingress.bind_traffic().await.unwrap().is_none());

    let ingress = ingress.with_traffic(true);
    let configuration = build(&ingress);
    let servers = &configuration["apps"]["http"]["servers"];
    for (server, environment, host) in [
        ("public", "env-public", "www.example.com"),
        ("private", "env-private", "admin.example.com"),
    ] {
        assert_eq!(
            servers[server]["logs"],
            json!({"default_logger_name":"traffic"})
        );
        // The probe endpoint isn't tagged, so it isn't counted.
        let routes = servers[server]["routes"].as_array().unwrap();
        assert_eq!(routes[0]["handle"][0]["handler"], "static_response");
        let handle = routes[1]["handle"].as_array().unwrap();
        assert_eq!(
            handle[..3],
            [
                json!({"handler":"log_append","key":"piqueld_environment","value":environment}),
                json!({"handler":"log_append","key":"piqueld_service","value":"web"}),
                json!({"handler":"log_append","key":"piqueld_hostname","value":host}),
            ]
        );
        assert_eq!(handle[3]["handler"], "reverse_proxy");
    }
    // HTTP servers only redirect, so they log nothing.
    assert!(servers["public_http"].get("logs").is_none());
    let logs = &configuration["logging"]["logs"];
    assert_eq!(
        logs["default"]["exclude"],
        json!(["http.log.access.traffic"])
    );
    assert_eq!(
        logs["traffic"]["writer"],
        json!({"output":"net","address":"unix//control/traffic.sock","soft_start":true})
    );

    ingress.bind_traffic().await.unwrap().unwrap();
    let socket = engine.directory.path().join("ingress/control/traffic.sock");
    assert!(socket.exists());
}

#[tokio::test]
async fn the_webhook_hostname_forwards_only_its_webhook_path_to_the_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let hooks = Hostname::parse("hooks.example.com").unwrap();
    let (engine, ingress) = private_ingress().await;
    let ingress = ingress.with_webhooks(Some(hooks.clone()));
    let configuration = ingress.build_configuration(&RoutingTable::new(), &[], None, None);
    let public = &configuration["apps"]["http"]["servers"]["public"];
    assert_eq!(
        public["tls_connection_policies"][0]["match"]["sni"],
        json!([hooks.as_str()])
    );
    let routes = public["routes"].as_array().unwrap();
    // The webhook path goes to the daemon's socket, bounded like the
    // daemon's own limit; every other path reaches the final 404.
    assert_eq!(routes.len(), 2);
    assert_eq!(
        routes[0]["match"],
        json!([{"host":[hooks.as_str()],"path":["/hooks/github/*"]}])
    );
    assert_eq!(
        routes[0]["handle"],
        json!([
            {"handler":"request_body","max_size":crate::api::WEBHOOK_BODY_LIMIT},
            {"handler":"reverse_proxy","upstreams":[{"dial":"unix//control/webhooks.sock"}]}
        ])
    );
    assert_eq!(routes[1]["handle"][0]["status_code"], 404);

    let listener = ingress.bind_webhooks().await.unwrap().unwrap();
    let path = engine
        .directory
        .path()
        .join("ingress/control/webhooks.sock");
    assert_eq!(
        listener.local_addr().unwrap().as_pathname(),
        Some(path.as_path())
    );
    // Only the daemon's user, which the gateway runs as, may connect.
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
        0
    );
    // Without managed ingress the socket is still served, for another proxy
    // running as the daemon's user, but the gateway routes nothing to it.
    drop(listener);
    let disabled = engine.ingress(false).with_webhooks(Some(hooks));
    assert!(disabled.bind_webhooks().await.unwrap().is_some());
    assert_eq!(disabled.webhook_hostname(), None);
    // Without a webhook hostname, there is no socket.
    let none = engine.ingress(true);
    assert!(none.bind_webhooks().await.unwrap().is_none());
}

#[tokio::test]
async fn the_dashboard_hostname_is_a_private_host_forwarded_to_the_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let dashboard = Hostname::parse("dashboard.example.com").unwrap();
    let (engine, ingress) = private_ingress().await;
    let ingress = ingress.with_private(&PrivateIngressConfig {
        dashboard_hostname: Some(dashboard.clone()),
        ..private_config()
    });
    let edge = edge();
    let configuration = ingress.build_configuration(&RoutingTable::new(), &[], Some(&edge), None);
    let servers = &configuration["apps"]["http"]["servers"];
    assert_eq!(
        servers["private"]["tls_connection_policies"][0]["match"]["sni"],
        json!([dashboard.as_str()])
    );
    assert_eq!(
        servers["public"]["tls_connection_policies"][0]["match"]["sni"],
        json!([])
    );
    // The gateway names each request's tailnet client, since one upstream
    // connection carries many.
    let route = &servers["private"]["routes"][0];
    assert_eq!(route["match"], json!([{"host":[dashboard.as_str()]}]));
    assert_eq!(
        route["handle"][0]["upstreams"],
        json!([{"dial":"unix//control/dashboard.sock"}])
    );
    assert_eq!(
        route["handle"][0]["headers"]["request"]["set"]["X-Forwarded-For"],
        json!(["{http.request.remote.host}"])
    );
    // Its certificate comes from DNS-01, like private routes'.
    assert_eq!(
        ingress.dns01_hostnames(&RoutingTable::new()),
        [dashboard.clone().into()].into()
    );
    // Until the private listener runs, it is served nowhere.
    let configuration = ingress.build_configuration(&RoutingTable::new(), &[], None, None);
    assert!(configuration["apps"]["http"]["servers"]["private"].is_null());

    let mut config = piqueld_config::DaemonConfig::default();
    config.server.data_dir = engine.directory.path().into();
    config.ingress.private.dashboard_hostname = Some(dashboard.clone());
    let socket = Ingress::bind_dashboard(&config).await.unwrap().unwrap();
    let path = engine
        .directory
        .path()
        .join("ingress/control/dashboard.sock");
    assert_eq!(
        socket.listener.local_addr().unwrap().as_pathname(),
        Some(path.as_path())
    );
    // Only the daemon's user, which the gateway runs as, may connect.
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(socket.hostname, dashboard);
}
