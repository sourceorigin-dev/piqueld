//! Journaled actions and health reporting, against a stub Docker Engine.
use super::*;
use piqueld_core::{
    EventActor, Phase,
    observability::{DiagnosticCode, EventScope},
};

#[tokio::test]
async fn journaled_requests_commit_intent_and_record_sanitized_failures() {
    let engine = Engine::failing().await;
    let ingress = engine.ingress(true);
    let journal = ingress
        .journal(Phase::IngressCreateNetwork, "edge")
        .await
        .expect("the journal records the phase");
    let labels = json!({});
    let result = journal
        .change(|docker| docker.create_bridge_network("edge", &labels))
        .await;
    journal
        .finish(result)
        .await
        .expect_err("the journal reports the failure");

    let events = engine.history().await;
    let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["action_started", "action_requested", "action_failed"]
    );
    let failed = &events[2];
    assert_eq!(failed.scope, EventScope::Daemon);
    assert_eq!(failed.error_code.as_deref(), Some("ingress_unavailable"));
    assert!(
        failed
            .diagnostic
            .as_ref()
            .expect("the value is present")
            .causes
            .contains(&"Docker returned HTTP status 500".to_owned())
    );
    assert!(
        failed
            .diagnostic
            .as_ref()
            .expect("the value is present")
            .causes
            .contains(&"Docker operation: create network".to_owned())
    );
}

/// Gateway passes credit the deployment they serve, but the DNS and
/// certificate loops, which run alongside them, are the daemon's own.
#[tokio::test]
async fn only_gateway_actions_credit_the_deployment_they_serve() {
    let engine = Engine::failing().await;
    let ingress = engine.ingress(true);
    let actor = Actor::System(piqueld_core::sync::SystemActor::SyncPoll);
    let app = application("blog", "blog.example.test", "blog");
    let (_, operation) = request_deployment(&engine.store, actor, app).await;
    *ingress.requester.lock().await = Some(operation);
    let gateway = ingress.journal(Phase::IngressConfigureRoutes, "gateway");
    gateway
        .await
        .expect("the task joins")
        .finish(Ok(()))
        .await
        .expect("finish succeeds");
    let records = ingress.journal_as(
        DiagnosticCode::DnsRecordsFailed,
        Phase::IngressDnsCreate,
        "blog",
    );
    records
        .await
        .expect("the task joins")
        .finish(Ok(()))
        .await
        .expect("finish succeeds");

    let events = engine.history().await;
    let actor = |phase: Phase| {
        let started = events
            .iter()
            .find(|event| event.kind == "action_started" && event.phase == Some(phase));
        started.expect("the work starts").actor.clone()
    };
    assert_eq!(
        actor(Phase::IngressConfigureRoutes),
        Some(EventActor::System {
            actor: piqueld_core::sync::SystemActor::SyncPoll
        })
    );
    assert_eq!(actor(Phase::IngressDnsCreate), None);
}

#[tokio::test]
async fn failed_gateway_sync_reports_health_without_response_details() {
    let engine = Engine::failing().await;
    let ingress = engine.ingress(true);
    ingress
        .synchronize()
        .await
        .expect_err("synchronizing fails");

    let events = engine.history().await;
    let [health] = events.as_slice() else {
        panic!("expected one health transition: {events:#?}")
    };
    assert_eq!(health.kind, "dependency_health_changed");
    assert_eq!(health.scope, EventScope::Daemon);
    assert_eq!(health.error_code.as_deref(), Some("ingress_unavailable"));
    let status = ingress.status().await;
    assert!(!status.healthy);
    assert_eq!(
        status.message,
        "Ingress unavailable: could not prepare the Caddy gateway. See daemon logs for details."
    );
}

#[tokio::test]
async fn old_docker_engine_is_named_in_ingress_health() {
    let engine = Engine::serving(|_| {
        axum::Router::new()
            .fallback(|| async { axum::Json(json!({"Version": "27.5.0", "ApiVersion": "1.47"})) })
    })
    .await;
    let ingress = engine.ingress(true);
    ingress
        .synchronize()
        .await
        .expect_err("synchronizing fails");
    assert_eq!(
        ingress.status().await.message,
        "Ingress unavailable: could not prepare the Caddy gateway: Docker Engine 27.5.0 with \
         API 1.47 is too old for ingress, which needs Engine 28+ with API 1.48+. See daemon \
         logs for details."
    );
}

/// Whether ingress or only private ingress is disabled, the apps node is
/// unconfirmed until Docker confirms it is gone, even when the gateway
/// failed first.
#[tokio::test]
async fn a_disabled_apps_node_is_unconfirmed_until_it_is_removed() {
    for (ingress, private, message) in [
        (true, false, "Removing the apps node is not confirmed"),
        (false, true, "Stopping the apps node is not confirmed"),
    ] {
        let engine = Engine::failing().await;
        let mut ingress = engine.ingress(ingress);
        if private {
            ingress = ingress.with_private(&private_config());
        }
        ingress
            .synchronize()
            .await
            .expect_err("synchronizing fails");
        let status = ingress.status().await.private;
        assert_eq!((status.enabled, status.healthy), (private, false));
        assert!(status.message.starts_with(message), "{}", status.message);
    }
}

#[tokio::test]
async fn an_unhealthy_apps_node_alone_makes_ingress_unavailable() {
    let (engine, ingress) = private_ingress().await;
    let private = PrivateIngressStatus {
        enabled: true,
        healthy: false,
        ..PrivateIngressStatus::default()
    };

    ingress.observe_availability(true, &private).await;

    let events = engine.history().await;
    let [health] = events.as_slice() else {
        panic!("expected one health transition: {events:#?}")
    };
    assert_eq!(health.kind, "dependency_health_changed");
    assert_eq!(health.error_code.as_deref(), Some("ingress_unavailable"));
}

/// A failure seen when one gateway pass ended keeps being observed while the
/// next pass hangs on Docker, so a sustained outage notifies even though no
/// pass ends within the observation gap.
#[tokio::test]
async fn a_failure_notifies_while_the_next_pass_is_slow() {
    use piqueld_core::observability::{DestinationKind, DestinationSettings};
    let engine = Engine::unused().await;
    let ingress = engine.ingress(true);
    let settings = DestinationSettings::new(DestinationKind::Json);
    (engine.store)
        .add_destination("test", "https://example.com/hook", settings)
        .await;
    let failed_at = now_ms();
    ingress
        .observe_availability(true, &PrivateIngressStatus::default())
        .await;
    for period in 1..=9 {
        ingress
            .repeat_availability(failed_at + period * 15_000)
            .await;
    }
    let deliveries = engine
        .store
        .deliveries(None, 100)
        .await
        .expect("the deliveries load")
        .items;
    assert_eq!(deliveries.len(), 1);
}

/// Each log relay starts with the next container, so a slow engine that
/// uses up the relay's budget on the first never starves the others.
#[tokio::test]
async fn log_relays_take_turns_starting_with_each_container() {
    let engine = Engine::unused().await;
    let ingress = engine.ingress(true);
    let first: Vec<String> = (0..4).map(|_| ingress.relay_order()[0].clone()).collect();
    let gateway = ingress.name.clone();
    assert_eq!(
        first,
        [
            gateway.clone(),
            ingress.node_name(),
            ingress.tunnel_name(),
            gateway
        ]
    );
}
