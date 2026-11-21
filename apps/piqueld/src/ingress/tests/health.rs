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
        .unwrap();
    let labels = json!({});
    let result = journal
        .change(|docker| docker.create_bridge_network("edge", &labels))
        .await;
    journal.finish(result).await.unwrap_err();

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
            .unwrap()
            .causes
            .contains(&"Docker returned HTTP status 500".to_owned())
    );
    assert!(
        failed
            .diagnostic
            .as_ref()
            .unwrap()
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
    gateway.await.unwrap().finish(Ok(())).await.unwrap();
    let records = ingress.journal_as(
        DiagnosticCode::DnsRecordsFailed,
        Phase::IngressDnsCreate,
        "blog",
    );
    records.await.unwrap().finish(Ok(())).await.unwrap();

    let events = engine.history().await;
    let actor = |phase: Phase| {
        let started = events
            .iter()
            .find(|event| event.kind == "action_started" && event.phase == Some(phase));
        started.unwrap().actor.clone()
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
    ingress.synchronize().await.unwrap_err();

    // The failure happened while reading Docker, so no action was journaled.
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
    ingress.synchronize().await.unwrap_err();
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
        ingress.synchronize().await.unwrap_err();
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
