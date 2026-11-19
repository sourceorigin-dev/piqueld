//! Which application networks the gateway keeps, attaches and detaches,
//! and its own edge network, against a stub Docker Engine.
use super::*;
use piqueld_core::resource::{APPLICATION_LABEL, INGRESS_LABEL, INSTANCE_LABEL, MANAGED_LABEL};

/// Disabled ingress removes its edge network once no gateway container is
/// left, but only its own installation's, and is disabled either way.
#[tokio::test]
async fn disabled_ingress_removes_its_own_edge_network() {
    for (owner, removed) in [(None, true), (Some("instance-other"), false)] {
        let deleted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let engine = Engine::serving(|store| {
            let (deleted, owner) = (
                Arc::clone(&deleted),
                owner.unwrap_or(store.instance_id().as_str()),
            );
            let network = json!({"Id":"edge-id","Labels":{
                "io.piqueld.managed":"true","io.piqueld.instance":owner,"io.piqueld.ingress":"true"
            }});
            axum::Router::new().fallback(move |method: Method, uri: axum::http::Uri| {
                let (deleted, network) = (Arc::clone(&deleted), network.clone());
                async move {
                    match (method, uri.path()) {
                        (Method::GET, path) if path.contains("/networks/") => {
                            axum::response::IntoResponse::into_response(axum::Json(network))
                        }
                        (Method::DELETE, path) => {
                            deleted
                                .lock()
                                .push(path.rsplit('/').next().unwrap().to_owned());
                            axum::response::IntoResponse::into_response(
                                hyper::StatusCode::NO_CONTENT,
                            )
                        }
                        _ => axum::response::IntoResponse::into_response(
                            hyper::StatusCode::NOT_FOUND,
                        ),
                    }
                }
            })
        })
        .await;
        engine.ingress(false).stop_gateway().await.unwrap();
        let expected: &[&str] = if removed { &["edge-id"] } else { &[] };
        assert_eq!(*deleted.lock(), expected);
    }
}

/// What the gateway keeps of an environment's acknowledged public route,
/// and why, when its ingress network is a managed overlay on `subnet`, or
/// on none, with or without a private listener.
async fn prepare_on_subnet(
    private: bool,
    subnet: Option<&'static str>,
) -> (Vec<ValidatedRoute>, Option<String>) {
    let environment = Arc::new(std::sync::OnceLock::<String>::new());
    let labelled = Arc::clone(&environment);
    let engine = Engine::serving(|store| {
        let instance = store.instance_id().to_string();
        axum::Router::new().fallback(move || {
            let network = json!({
                "Labels":{
                    MANAGED_LABEL:"true",INSTANCE_LABEL:instance,APPLICATION_LABEL:labelled.get()
                },
                "Driver":"overlay","Attachable":true,
                "IPAM":{"Config":subnet.map_or_default(|subnet| json!([{"Subnet":subnet}]))}
            });
            async move { axum::Json(network) }
        })
    })
    .await;
    let mut ingress = engine.ingress(true);
    if private {
        ingress = ingress.with_private(&private_config());
    }
    let app = application("public", "www.example.com", "body");
    let (id, _) = request_deployment(&engine.store, Actor::Daemon, app).await;
    environment.set(id.to_string()).unwrap();
    let store = &engine.store;
    let route = routes(Visibility::Public, "www.example.com");
    store.stage_routes(&id, &route, true, None).await.unwrap();
    let table = store.routing_table().await.unwrap();
    store.acknowledge_routes(&table).await.unwrap();
    let (prepared, _, mut failures) = ingress.prepare_routes(&table).await.unwrap();
    (prepared[&id].clone(), failures.remove(&id))
}

#[tokio::test]
async fn ingress_networks_overlapping_the_tailnet_lose_their_routes_with_a_private_listener() {
    for (private, subnet) in [(true, "10.0.3.0/24"), (false, "100.64.5.0/24")] {
        let (routes, failure) = Box::pin(prepare_on_subnet(private, Some(subnet))).await;
        assert_eq!((routes.len(), failure), (1, None), "{subnet}");
    }
    // A network without a subnet can't be checked, so it fails closed: its
    // accepted route is retained like any unverified network's, and the
    // gateway detaches the network (see the ordering test below).
    let (routes, failure) = Box::pin(prepare_on_subnet(true, None)).await;
    assert_eq!(routes.len(), 1);
    assert!(
        failure
            .as_deref()
            .is_some_and(|failure| failure.contains("no subnet")),
        "{failure:?}"
    );
    // Its containers could pass for tailnet clients on the private listener,
    // so even its acknowledged route is withdrawn and the network detached.
    let (routes, failure) = Box::pin(prepare_on_subnet(true, Some("100.64.5.0/24"))).await;
    assert_eq!(routes, []);
    assert!(
        failure.as_deref().is_some_and(
            |failure| failure.starts_with("ingress network subnet 100.64.5.0/24 overlaps")
        ),
        "{failure:?}"
    );
}

/// Networks that could pass their containers for tailnet clients, on a
/// tailnet subnet or on none, are detached before Caddy loads a
/// configuration, even while an environment's routes are retained; other
/// stale networks only after.
#[tokio::test]
async fn networks_exposing_the_private_listener_are_detached_before_caddy_reloads() {
    let id = EnvironmentId::parse("env-retained").unwrap();
    let overlap = piqueld_core::DockerNetworkName::for_ingress(&id).to_string();
    let requests = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let (docker, exposed) = (Arc::clone(&requests), overlap.clone());
    let engine = Engine::serving(|store| {
        let labels = json!({
            MANAGED_LABEL:"true",INSTANCE_LABEL:store.instance_id().as_str(),INGRESS_LABEL:"true"
        });
        axum::Router::new().fallback(move |request: axum::extract::Request| {
            let path = request.uri().path().to_owned();
            docker.lock().push(format!("{} {path}", request.method()));
            let body = if path.contains("/containers/") {
                json!({
                    "Config":{"Labels":labels.clone()},
                    "NetworkSettings":{"Networks":{exposed.as_str():{},"unknown":{},"stale":{}}}
                })
            } else if path.contains(exposed.as_str()) {
                json!({"IPAM":{"Config":[{"Subnet":"100.64.5.0/24"}]}})
            } else if path.contains("unknown") {
                json!({"IPAM":{"Config":[]}})
            } else {
                json!({"IPAM":{"Config":[{"Subnet":"10.0.3.0/24"}]}})
            };
            async move { axum::Json(body) }
        })
    })
    .await;
    let control = engine.directory.path().join("ingress/control");
    std::fs::create_dir_all(&control).unwrap();
    let admin = tokio::net::UnixListener::bind(control.join("admin.sock")).unwrap();
    let caddy = Arc::clone(&requests);
    let admin_api = axum::Router::new().fallback(move |request: axum::extract::Request| {
        caddy.lock().push(format!(
            "caddy {} {}",
            request.method(),
            request.uri().path()
        ));
        async { axum::Json(json!({})) }
    });
    let server = tokio::spawn(async move { axum::serve(admin, admin_api).await });
    let ingress = engine.ingress(true).with_private(&private_config());

    // Retained routes, as for a network failing its check for another reason.
    let table = [(id, routes(Visibility::Public, "www.example.com"))].into();
    ingress
        .configure_gateway(&table, &BTreeSet::new())
        .await
        .unwrap();
    server.abort();
    let log = requests.lock().clone();
    let position = |entry: &str| {
        log.iter()
            .position(|request| request.ends_with(entry))
            .unwrap_or_else(|| panic!("{entry} in {log:#?}"))
    };
    let load = position("caddy POST /load");
    assert!(position(&format!("/networks/{overlap}/disconnect")) < load);
    assert!(position("/networks/unknown/disconnect") < load);
    assert!(load < position("/networks/stale/disconnect"));
}
