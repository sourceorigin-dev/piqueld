//! Behavioral tests for durable diagnostics, deletion, crash recovery and delivery policies.
use crate::testing::{credential, template, user};
use crate::*;
use crate::{Mutation, MutationResponse};
use piqueld_core::Phase;
use piqueld_core::observability::{
    ApplicationSelection, DeliveryState, DestinationKind, DestinationName, DestinationRequest,
    DestinationSettings, Diagnostic, EventFilter, EventScope, NotificationCategory,
};
use piqueld_core::wait;

async fn application(store: &Store) -> Operation {
    named_application(store, "observable").await
}
/// The application whose environment `operation` runs on.
async fn application_of(store: &Store, operation: &Operation) -> ApplicationId {
    let environment = store
        .get(&operation.environment_id)
        .await
        .expect("the store can get");
    environment.environment.application_id
}
/// Saves and deploys an empty application, returning its running operation.
async fn named_application(store: &Store, name: &str) -> Operation {
    let saved = store.save_template(template(name, "[spec]"), true).await;
    let operation_id = saved.operation_id.expect("the store can operation id");
    store.mark_running(&operation_id).await;
    store
        .operation(&operation_id)
        .await
        .expect("the store can operation")
}
/// Settings receiving every application, preview and category, except
/// security events, which destination changes raise.
fn settings() -> DestinationSettings {
    let mut settings = DestinationSettings {
        previews: true,
        ..DestinationSettings::new(DestinationKind::Json)
    };
    settings.categories.remove(&NotificationCategory::Security);
    settings
}
/// Adds the destination `test` with [`settings`].
async fn notify(store: &Store) {
    let url = "https://example.com/private-token";
    store.add_destination("test", url, settings()).await;
}
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn interrupted_actions_and_diagnostics_survive_restart_and_scope_controls_deletion() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let path = temp.path().join("db");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    let op = application(&store).await;
    let action = store
        .begin_action(Some(&op.id), Phase::EnsureService, Some("web"))
        .await
        .expect("the store can begin action");
    store
        .action_request(&action, 1)
        .await
        .expect("the store can action request");
    drop(store);
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    store
        .interrupt_actions(None)
        .await
        .expect("the store can interrupt actions");
    let history = store
        .filtered_events(
            &EventFilter {
                action_id: Some(action.id.clone()),
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            100,
        )
        .await
        .expect("the store can filtered events");
    assert_eq!(
        history
            .items
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "action_started",
            "action_requested",
            "action_outcome_unknown"
        ]
    );
    let diagnostic = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::DockerUnavailable,
        "Docker became unreachable".into(),
    );
    let failure = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::ServiceUpdateFailed,
        "Service failed".into(),
    );
    for diagnostic in [&diagnostic, &failure] {
        let daemon = Attribution::default();
        let environment = Some(&op.environment_id);
        store
            .record_diagnostic(diagnostic, None, environment, daemon)
            .await
            .expect("the store can record diagnostic");
    }
    let (MutationResponse::Deleted(deleted), _) = store
        .accept(
            Actor::Daemon,
            Mutation::DeleteApplication {
                id: application_of(&store, &op).await,
                environments: Vec::new(),
            },
            None,
            true,
            None,
        )
        .await
        .expect("the store can accept")
    else {
        panic!("delete");
    };
    let delete = &deleted.operations[0];
    store.mark_running(&delete.operation_id).await;
    store
        .finish_delete_operation(
            &store
                .operation(&delete.operation_id)
                .await
                .expect("the store can operation"),
        )
        .await
        .expect("the store can finish delete operation");
    assert!(matches!(
        store.diagnostic(&failure.id).await,
        Err(StoreError::NotFound)
    ));
    let retained = store
        .diagnostic(&diagnostic.id)
        .await
        .expect("the store can diagnostic");
    assert_eq!(retained.scope, EventScope::Daemon);
    assert_eq!(retained.environment_id, Some(op.environment_id));
    assert!(
        store
            .events(None, None, 100)
            .await
            .expect("the store can events")
            .items
            .iter()
            .all(|event| event.scope == EventScope::Daemon)
    );
}
#[tokio::test]
async fn retries_keep_cause_and_outbox_deduplicates_until_recovery() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    for _ in 0..2 {
        fail(&store, &op).await;
        rerun(&store, &op).await;
    }
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .len(),
        1
    );
    let failed = store
        .filtered_events(
            &EventFilter {
                errors_only: true,
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            100,
        )
        .await
        .expect("the store can filtered events");
    assert_eq!(failed.items.len(), 2);
    assert!(failed.items.iter().all(|event| event.diagnostic.is_some()));
    succeed(&store, &op).await;
    let deliveries = store
        .deliveries(None, 100)
        .await
        .expect("the store can deliveries");
    assert_eq!(deliveries.items.len(), 2);
    assert_eq!(deliveries.items[0].category, NotificationCategory::Recovery);
    let cursor = store
        .deliveries(None, 1)
        .await
        .expect("the store can deliveries")
        .next_cursor
        .expect("the store can next cursor");
    assert_eq!(cursor, format!("v1:{}", deliveries.items[0].id));
    let older = store
        .deliveries(Some(&cursor), 1)
        .await
        .expect("the store can deliveries");
    assert_eq!(older.items[0].id, deliveries.items[1].id);
    let bare = store.deliveries(Some(&deliveries.items[0].id), 1).await;
    assert!(matches!(bare, Err(StoreError::InvalidInput)));
}

#[tokio::test]
async fn closed_incidents_cannot_be_retried_after_failed_delivery() {
    for failed_before_close in [false, true] {
        let (_temp, store) = Store::temporary().await;
        notify(&store).await;
        let op = application(&store).await;
        fail(&store, &op).await;
        let (failure, _) = store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .expect("a delivery is pending");
        let state = if failed_before_close {
            DeliveryState::Failed
        } else {
            DeliveryState::Pending
        };
        store
            .complete_delivery(&failure.id, state, Some("Receiver rejected alert"), 3600)
            .await
            .expect("the store can complete delivery");
        if failed_before_close {
            store
                .retry_delivery(Actor::Daemon, &failure.id)
                .await
                .expect("the store can retry delivery");
            store
                .complete_delivery(&failure.id, DeliveryState::Failed, Some("Rejected"), 0)
                .await
                .expect("the store can complete delivery");
        }
        rerun(&store, &op).await;
        succeed(&store, &op).await;
        if !failed_before_close {
            store
                .complete_delivery(&failure.id, DeliveryState::Failed, Some("Rejected"), 0)
                .await
                .expect("the store can complete delivery");
            assert!(
                store
                    .claim_delivery()
                    .await
                    .expect("the delivery claims")
                    .is_none()
            );
        }
        assert!(matches!(
            store.retry_delivery(Actor::Daemon, &failure.id).await,
            Err(StoreError::Retry(RetryError::Closed))
        ));
    }
}

#[tokio::test]
async fn failed_recovery_cannot_be_retried_after_its_incident_reopens() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    let finish = async |succeeded: bool| {
        if succeeded {
            succeed(&store, &op).await;
        } else {
            fail(&store, &op).await;
        }
        let (delivery, _) = store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .expect("a delivery is pending");
        rerun(&store, &op).await;
        delivery
    };
    let failure = finish(false).await;
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    let recovery = finish(true).await;
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    store
        .complete_delivery(&recovery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    store
        .retry_delivery(Actor::Daemon, &recovery.id)
        .await
        .expect("the store can retry delivery");
    store
        .complete_delivery(&recovery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    finish(false).await;
    assert!(matches!(
        store.retry_delivery(Actor::Daemon, &recovery.id).await,
        Err(StoreError::Retry(RetryError::Reopened))
    ));
}

/// Opens a daemon incident whose delivery to `test` fails, returning the
/// delivery's ID.
async fn failed_daemon_delivery(store: &Store) -> String {
    notify(store).await;
    let diagnostic = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::InternalError,
        "Daemon failure".into(),
    );
    store
        .record_diagnostic(&diagnostic, None, None, Attribution::default())
        .await
        .expect("the store can record diagnostic");
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    let (delivery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(&delivery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    delivery.id
}
/// An open daemon incident's failed delivery can be retried; retry names why
/// it refuses one that has not failed, or whose destination no longer
/// receives it.
#[tokio::test]
async fn retry_refusals_name_their_reason() {
    let (_temp, store) = Store::temporary().await;
    let delivery = failed_daemon_delivery(&store).await;
    let retry = || store.retry_delivery(Actor::Daemon, &delivery);
    assert!(matches!(
        store.retry_delivery(Actor::Daemon, "missing").await,
        Err(StoreError::NotFound)
    ));
    retry().await.expect("the store can retry");
    assert!(matches!(
        retry().await,
        Err(StoreError::Retry(RetryError::NotFailed(
            DeliveryState::Pending
        )))
    ));
    store
        .complete_delivery(&delivery, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    put(
        &store,
        &DestinationSettings {
            enabled: false,
            ..settings()
        },
        None,
    )
    .await;
    assert!(matches!(
        retry().await,
        Err(StoreError::Retry(RetryError::NotReceived))
    ));
}
/// As after a downgrade across a category a newer version added.
#[tokio::test]
async fn an_undeliverable_delivery_does_not_block_the_next() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    for code in [
        piqueld_core::observability::DiagnosticCode::InternalError,
        piqueld_core::observability::DiagnosticCode::DnsRecordsFailed,
    ] {
        let diagnostic = Diagnostic::new(new_id("diagnostic"), code, "Daemon failure".into());
        store
            .record_diagnostic(&diagnostic, None, None, Attribution::default())
            .await
            .expect("the store can record diagnostic");
    }
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    let poisoned: String = sqlx::query_scalar(
        "UPDATE notification_deliveries SET category='retired' WHERE id=(SELECT id FROM \
         notification_deliveries ORDER BY next_attempt_ms,id LIMIT 1) RETURNING id",
    )
    .fetch_one(&store.pool)
    .await
    .expect("the row is present");

    let (claimed, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");

    assert_ne!(claimed.id, poisoned);
    let settled: (String, Option<String>) =
        sqlx::query_as("SELECT state,last_error FROM notification_deliveries WHERE id=?1")
            .bind(&poisoned)
            .fetch_one(&store.pool)
            .await
            .expect("the row is present");
    assert_eq!(
        settled,
        (
            "failed".into(),
            Some("Unknown notification category".into())
        )
    );
    let listed = store
        .deliveries(None, 100)
        .await
        .expect("the store can deliveries")
        .items;
    assert_eq!(
        listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        [claimed.id.as_str()]
    );
    assert!(matches!(
        store.retry_delivery(Actor::Daemon, &poisoned).await,
        Err(StoreError::Retry(RetryError::UnknownCategory))
    ));
}
/// Replaces the settings of the destination `test`, keeping its URL unless
/// `url` is given.
async fn put(store: &Store, settings: &DestinationSettings, url: Option<&str>) {
    let request = DestinationRequest {
        settings: settings.clone(),
        url: url.map(Into::into),
    };
    store
        .put_notification_destination(
            Actor::Daemon,
            &DestinationName::parse("test").expect("the destination name is valid"),
            request,
        )
        .await
        .expect("the store can put notification destination");
}
/// Delivers every due delivery, returning their destinations and categories.
async fn drain(store: &Store) -> Vec<(String, NotificationCategory)> {
    let mut delivered = Vec::new();
    while let Some((delivery, _)) = store.claim_delivery().await.expect("the delivery claims") {
        store
            .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
            .await
            .expect("the store can complete delivery");
        delivered.push((delivery.destination, delivery.category));
    }
    delivered
}
/// Fails `op`'s deployment and processes the failure's notifications.
async fn fail(store: &Store, op: &Operation) {
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("service_update_failed", "Update paused")),
        )
        .await
        .expect("the store can transition operation");
    store
        .process_notifications()
        .await
        .expect("the notifications process");
}
/// Succeeds `op`'s deployment and processes the success's notifications.
async fn succeed(store: &Store, op: &Operation) {
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .expect("the store can transition operation");
    store
        .process_notifications()
        .await
        .expect("the notifications process");
}
/// Retries `op` once it finished, marking the retry running.
async fn rerun(store: &Store, op: &Operation) {
    let finished = store
        .operation(&op.id)
        .await
        .expect("the store can operation");
    store
        .retry_operation(&finished)
        .await
        .expect("the store can retry operation");
    store.mark_running(&op.id).await;
}
#[tokio::test]
async fn disabling_cancels_pending_and_reenabling_never_replays() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let mut settings = settings();
    settings.enabled = false;
    put(&store, &settings, None).await;
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items[0]
            .state,
        DeliveryState::Cancelled
    );
    settings.enabled = true;
    put(&store, &settings, None).await;
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
}
/// A new URL makes the destination a new receiver, which is never sent the
/// recovery of a failure the old one received.
#[tokio::test]
async fn new_urls_receive_no_recovery_of_earlier_failures() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (failure, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    let settings = settings();
    put(&store, &settings, Some("https://example.com/private-token")).await;
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none(),
        "unchanged URL"
    );
    put(&store, &settings, Some("https://example.com/new-token")).await;
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    let history = store
        .deliveries(None, 100)
        .await
        .expect("the store can deliveries")
        .items;
    assert!(
        history
            .iter()
            .all(|d| d.category != NotificationCategory::Recovery)
    );
}
/// Enabling a destination again keeps the failures it was sent, so their
/// recovery still reaches it.
#[tokio::test]
async fn reenabled_destinations_still_receive_recoveries() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (failure, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    let mut settings = settings();
    settings.enabled = false;
    put(&store, &settings, None).await;
    settings.enabled = true;
    put(&store, &settings, None).await;
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    let (recovery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(recovery.category, NotificationCategory::Recovery);
}
/// Destinations receive notifications about their applications, previews
/// only when they ask, and daemon notifications whatever their applications.
#[tokio::test]
async fn destinations_receive_their_applications_and_daemon_notifications() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let watched = named_application(&store, "watched").await;
    let other = named_application(&store, "other").await;
    let mut settings = settings();
    settings.previews = false;
    settings.applications =
        ApplicationSelection::Only([application_of(&store, &watched).await].into());
    fail(&store, &other).await;
    put(&store, &settings, None).await;
    let cancelled = store
        .deliveries(None, 100)
        .await
        .expect("the store can deliveries")
        .items
        .remove(0);
    assert_eq!(cancelled.state, DeliveryState::Cancelled);
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    fail(&store, &watched).await;
    store
        .record_daemon_started("0.1.0")
        .await
        .expect("the store can record daemon started");
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    let mut categories = Vec::new();
    while let Some((delivery, _)) = store.claim_delivery().await.expect("the delivery claims") {
        categories.push(delivery.category);
        store
            .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
            .await
            .expect("the store can complete delivery");
    }
    categories.sort();
    assert_eq!(
        categories,
        [
            NotificationCategory::DeploymentFailures,
            NotificationCategory::DaemonStarted
        ]
    );
}
#[tokio::test]
async fn sustained_observation_requires_continuity_and_emits_one_recovery() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    for timestamp in [1000, 31_000, 61_000, 91_000] {
        store
            .observe_condition("docker_unavailable", None, true, timestamp, 45_000)
            .await
            .expect("the store can observe condition");
    }
    assert!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .is_empty()
    );
    store
        .observe_condition("docker_unavailable", None, true, 121_000, 45_000)
        .await
        .expect("the store can observe condition");
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .len(),
        1
    );
    store
        .observe_condition("docker_unavailable", None, false, 151_000, 45_000)
        .await
        .expect("the store can observe condition");
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .len(),
        2
    );
    for timestamp in [200_000, 400_000, 430_000] {
        store
            .observe_condition("docker_unavailable", None, true, timestamp, 45_000)
            .await
            .expect("the store can observe condition");
    }
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .len(),
        2
    );
}
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn open_incidents_survive_retention_until_recovery_is_delivered() {
    for daemon in [false, true] {
        let temp = tempfile::tempdir().expect("the temp dir is created");
        let path = temp.path().join("db");
        let retention = piqueld_core::RetentionConfig {
            daemon_event_days: 1,
            ..Default::default()
        };
        let store = Store::open_unsynced(&path)
            .await
            .expect("the store opens")
            .with_observability(
                &retention,
                &piqueld_core::ImagesConfig::default(),
                Duration::from_secs(60),
            );
        notify(&store).await;
        let op = application(&store).await;
        let application = (!daemon).then_some(op.environment_id.as_str());
        let key = application.unwrap_or("docker_unavailable");
        let started = now_ms() - 2 * 86_400_000;
        let cutoff = now_ms() - 86_400_000;
        store
            .observe_condition(key, application, true, started, 45_000)
            .await
            .expect("the store can observe condition");
        store
            .prune_events(cutoff)
            .await
            .expect("the store can prune events");
        store
            .prune_daemon_events()
            .await
            .expect("the store can prune daemon events");
        for elapsed in [30_000, 60_000, 90_000, 120_000] {
            store
                .observe_condition(key, application, true, started + elapsed, 45_000)
                .await
                .expect("the store can observe condition");
        }
        let (failure, _) = store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .expect("a delivery is pending");
        store
            .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
            .await
            .expect("the store can complete delivery");
        store
            .prune_events(cutoff)
            .await
            .expect("the store can prune events");
        store
            .prune_daemon_events()
            .await
            .expect("the store can prune daemon events");
        store
            .event(failure.event_id)
            .await
            .expect("`event` should succeed");
        drop(store);

        let store = Store::open_unsynced(&path)
            .await
            .expect("the store opens")
            .with_observability(
                &retention,
                &piqueld_core::ImagesConfig::default(),
                Duration::from_secs(60),
            );
        store
            .restart_failure_windows()
            .await
            .expect("the store can restart failure windows");
        store
            .observe_condition(key, application, false, started + 150_000, 45_000)
            .await
            .expect("the store can observe condition");
        store
            .prune_events(cutoff)
            .await
            .expect("the store can prune events");
        store
            .prune_daemon_events()
            .await
            .expect("the store can prune daemon events");
        let (recovery, _) = store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .expect("a delivery is pending");
        assert_eq!(recovery.category, NotificationCategory::Recovery);
        assert_eq!(recovery.destination, failure.destination);
        store
            .event(failure.event_id)
            .await
            .expect("`event` should succeed");
        store
            .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
            .await
            .expect("the store can complete delivery");
        assert!(
            store
                .claim_delivery()
                .await
                .expect("the delivery claims")
                .is_none()
        );

        store
            .prune_events(cutoff)
            .await
            .expect("the store can prune events");
        store
            .prune_daemon_events()
            .await
            .expect("the store can prune daemon events");
        assert!(matches!(
            store.event(failure.event_id).await,
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.event(recovery.event_id).await,
            Err(StoreError::NotFound)
        ));
        assert!(
            store
                .deliveries(None, 100)
                .await
                .expect("the store can deliveries")
                .items
                .is_empty()
        );
    }
}

#[tokio::test]
async fn pruning_marks_stream_gaps_and_analytics_coverage() {
    let (_temp, store) = Store::temporary().await;
    let op = application(&store).await;
    succeed(&store, &op).await;
    let last = store
        .filtered_events(
            &EventFilter {
                descending: true,
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            1,
        )
        .await
        .expect("the store can filtered events")
        .items[0]
        .id;
    let quiet = EventFilter {
        kind: Some("no_such_kind".into()),
        ..EventFilter::default()
    };
    let (items, checkpoint) = store
        .stream_events(&quiet, &Visibility::ALL, 0, 10)
        .await
        .expect("the store can stream events");
    assert!(items.is_empty());
    assert_eq!(checkpoint, last);
    store
        .prune_events(now_ms() + 1)
        .await
        .expect("the store can prune events");
    assert!(matches!(
        store
            .stream_events(&EventFilter::default(), &Visibility::ALL, last - 1, 1)
            .await,
        Err(StoreError::HistoryExpired)
    ));
    store
        .stream_events(&quiet, &Visibility::ALL, checkpoint, 10)
        .await
        .expect("`` should succeed");
    let analytics = store
        .deployment_analytics(None, &Visibility::ALL, 0, now_ms())
        .await
        .expect("the store can deployment analytics");
    assert!(analytics.incomplete);
    assert_eq!(analytics.succeeded, 1);
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn recovery_waits_for_its_destination_failure_across_restart_and_pruning() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let path = temp.path().join("db");
    let store = Store::open_unsynced(&path).await.expect("the store opens");
    notify(&store).await;
    let url = "https://example.com/second";
    store.add_destination("second", url, settings()).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (delayed, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(
            &delayed.id,
            DeliveryState::Pending,
            Some("Receiver unavailable"),
            3600,
        )
        .await
        .expect("the store can complete delivery");
    let (acknowledged, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(&acknowledged.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    drop(store);

    let store = Store::open_unsynced(&path).await.expect("the store opens");
    let url = "https://example.com/new";
    store
        .add_destination("new-destination", url, settings())
        .await;
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    let (recovery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    assert_eq!(recovery.destination, acknowledged.destination);
    store
        .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    assert_eq!(
        store
            .deliveries(None, 100)
            .await
            .expect("the store can deliveries")
            .items
            .len(),
        4
    );
    drop(store);

    let store = Store::open_unsynced(&path).await.expect("the store opens");
    store
        .restart_failure_windows()
        .await
        .expect("the store can restart failure windows");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    store
        .complete_delivery(&delayed.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    store
        .prune_events(now_ms() + 1)
        .await
        .expect("the store can prune events");
    store
        .event(delayed.event_id)
        .await
        .expect("`event` should succeed");
    let (recovery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    assert_eq!(recovery.destination, delayed.destination);
    store
        .event(recovery.event_id)
        .await
        .expect("`event` should succeed");
}

#[tokio::test]
async fn undelivered_failures_expire_and_cancel_recovery_even_when_nothing_is_due() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (failure, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(
            &failure.id,
            DeliveryState::Pending,
            Some("Unavailable"),
            3600,
        )
        .await
        .expect("the store can complete delivery");
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    sqlx::query!(
        "UPDATE notification_deliveries SET retry_started_at_ms=0 WHERE id=?1",
        failure.id
    )
    .execute(&store.pool)
    .await
    .expect("the statement executes");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    let deliveries = store
        .deliveries(None, 100)
        .await
        .expect("the store can deliveries")
        .items;
    assert_eq!(deliveries.len(), 2);
    assert_eq!(
        deliveries
            .iter()
            .find(|d| d.id == failure.id)
            .expect("the item is found")
            .state,
        DeliveryState::Failed
    );
    assert_eq!(
        deliveries
            .iter()
            .find(|d| d.category == NotificationCategory::Recovery)
            .expect("the item is found")
            .state,
        DeliveryState::Cancelled
    );
}

#[tokio::test]
async fn recovery_waits_for_all_pending_failure_categories_at_the_same_destination() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    for code in ["git_build_failed", "service_update_failed"] {
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some((code, "Attempt failed")),
            )
            .await
            .expect("the store can transition operation");
        store
            .process_notifications()
            .await
            .expect("the notifications process");
        rerun(&store, &op).await;
    }
    let (first, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(&first.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    let (second, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    store
        .complete_delivery(
            &second.id,
            DeliveryState::Pending,
            Some("Unavailable"),
            3600,
        )
        .await
        .expect("the store can complete delivery");
    succeed(&store, &op).await;
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    store
        .complete_delivery(&second.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    let (recovery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    store
        .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    assert!(matches!(
        store.retry_delivery(Actor::Daemon, &second.id).await,
        Err(StoreError::Retry(RetryError::Recovered))
    ));
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
}

/// History, the event stream, builds, and analytics only include the
/// applications a caller may read; daemon history needs its own visibility.
#[tokio::test]
async fn visibility_limits_history_builds_and_analytics() {
    use piqueld_core::access::Scope;
    let (_temp, store) = Store::temporary().await;
    let mut ids = Vec::new();
    for name in ["blog", "shop"] {
        let op = named_application(&store, name).await;
        succeed(&store, &op).await;
        let source = piqueld_core::manifest::ValidatedSource::Image {
            image: "example/web:1".into(),
        };
        store
            .start_build(&op.environment_id, &op.id, "web", &source, None)
            .await
            .expect("the store can start build");
        ids.push((application_of(&store, &op).await, op.environment_id));
    }
    let daemon = Diagnostic::from_recorded_code(
        "diagnostic-daemon".into(),
        "docker_unavailable",
        "down".into(),
    );
    store
        .record_diagnostic(&daemon, None, None, Attribution::default())
        .await
        .expect("the store can record diagnostic");
    let blog = Scope::one(ids[0].0.clone());
    let only_blog = |event: &piqueld_core::Event| {
        event.scope == EventScope::Application && event.application_id.as_ref() == Some(&ids[0].0)
    };
    for daemon in [false, true] {
        let visible = Visibility {
            applications: blog.clone(),
            daemon,
        };
        let page = store
            .filtered_events(&EventFilter::default(), &visible, None, 100)
            .await
            .expect("the store can filtered events")
            .items;
        let (streamed, _) = store
            .stream_events(&EventFilter::default(), &visible, 0, 100)
            .await
            .expect("the store can stream events");
        for events in [page, streamed] {
            assert!(events.iter().any(only_blog));
            assert!(
                events
                    .iter()
                    .all(|event| only_blog(event) || (daemon && event.scope == EventScope::Daemon))
            );
            assert_eq!(
                events.iter().any(|event| event.scope == EventScope::Daemon),
                daemon
            );
        }
    }
    let builds = store
        .builds(None, None, &blog, None, 50)
        .await
        .expect("the store can builds")
        .items;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].environment_id, ids[0].1.as_str());
    let analytics = |applications: Scope, daemon: bool| {
        let store = &store;
        async move {
            let visible = Visibility {
                applications,
                daemon,
            };
            let analytics = store
                .deployment_analytics(None, &visible, 0, now_ms())
                .await
                .expect("the store can deployment analytics");
            let daemon_failure = analytics
                .failures
                .iter()
                .any(|failure| failure.code == "docker_unavailable");
            (analytics.succeeded, daemon_failure)
        }
    };
    assert_eq!(analytics(blog.clone(), false).await, (1, false));
    assert_eq!(analytics(Scope::All, false).await, (2, false));
    assert_eq!(analytics(Scope::All, true).await, (2, true));
}

/// A refused request from `peer`, as the audit middleware records it.
fn refused(peer: &str) -> NewAuditEvent {
    NewAuditEvent {
        action: "GET /api/v1/applications/{id}".into(),
        outcome: piqueld_core::audit::AuditOutcome::Denied,
        status: 404,
        origin: AuditOrigin {
            peer: Some(peer.into()),
            ..AuditOrigin::default()
        },
        application_id: None,
        environment_id: None,
        permission: None,
    }
}

/// A refusal burst and a credential appearing on a new address each raise
/// one security event, delivered as a `security` notification; a first
/// address and further refusals within the burst cooldown do not. Failed
/// security deliveries can be retried.
#[tokio::test]
async fn refusal_bursts_and_new_addresses_notify_as_security() {
    let (_temp, store) = Store::temporary().await;
    let url = "https://example.com/private-token";
    let settings = DestinationSettings::new(DestinationKind::Json);
    store.add_destination("test", url, settings).await;
    for _ in 0..25 {
        store
            .record_audit(&refused("203.0.113.9"))
            .await
            .expect("the store can record audit");
    }
    let rearm =
        "UPDATE events SET created_at_ms=created_at_ms-660000 WHERE kind='access_denial_burst'";
    sqlx::query(rearm)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    store
        .record_audit(&refused("203.0.113.9"))
        .await
        .expect("the store can record audit");
    store
        .seed_auth_user(&user("alice"), &piqueld_core::access::Grants::admin())
        .await;
    let laptop = NewCredential {
        secret_hash: "hash".into(),
        name: "piquelctl",
        ..credential("laptop", CredentialKind::Cli)
    };
    store
        .insert_credential("alice", &laptop)
        .await
        .expect("the store can insert credential");
    for address in ["192.0.2.1", "192.0.2.1", "198.51.100.7"] {
        let note = store.note_credential_address("laptop", "alice", address);
        note.await.expect("the store can note");
    }
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    let deliveries = store
        .deliveries(None, 10)
        .await
        .expect("the store can deliveries")
        .items;
    let failed = &deliveries[0].id;
    let fail = "UPDATE notification_deliveries SET state='failed' WHERE id=?1";
    sqlx::query(fail)
        .bind(failed)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    store
        .retry_delivery(Actor::Daemon, failed)
        .await
        .expect("the store can retry delivery");
    let mut notified = Vec::new();
    for delivery in deliveries {
        assert_eq!(delivery.category, NotificationCategory::Security);
        let event = store
            .event(delivery.event_id)
            .await
            .expect("the store can event");
        notified.push(event.message.unwrap_or_default());
    }
    notified.sort();
    let burst = "20 requests refused within a minute from 203.0.113.9 as an anonymous caller";
    assert_eq!(
        notified,
        [
            burst,
            burst,
            "alice's cli \"piquelctl\" was used from a new address: 198.51.100.7",
        ]
    );
}

/// A security event raised by a system actor is attributed to it, not to the
/// daemon.
#[tokio::test]
async fn security_events_record_their_system_actor() {
    let (_temp, store) = Store::temporary().await;
    let system = piqueld_core::sync::SystemActor::SyncPoll;
    let mut db = store
        .pool
        .acquire()
        .await
        .expect("a connection is acquired");
    let by = Actor::System(system).attribution();
    Store::security_event_on(&mut db, SecurityEvent::AdminGranted, "granted", by)
        .await
        .expect("the store can security event on");
    let id = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut *db)
        .await
        .expect("the row is present");
    let event = store.event(id).await.expect("the store can event");
    let actor = piqueld_core::event::EventActor::System { actor: system };
    assert_eq!(event.actor, Some(actor));
}

/// Each daemon start notifies as `daemon_started`, naming the previous
/// version only when it changed.
#[tokio::test]
async fn daemon_starts_notify_with_version_changes() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    for version in ["0.1.0", "0.1.0", "0.2.0"] {
        store
            .record_daemon_started(version)
            .await
            .expect("the store can record daemon started");
    }
    store
        .process_notifications()
        .await
        .expect("the notifications process");
    let mut notified = Vec::new();
    for delivery in store
        .deliveries(None, 10)
        .await
        .expect("the store can deliveries")
        .items
    {
        assert_eq!(delivery.category, NotificationCategory::DaemonStarted);
        let event = store
            .event(delivery.event_id)
            .await
            .expect("the store can event");
        notified.push(event.message.unwrap_or_default());
    }
    notified.sort();
    assert_eq!(
        notified,
        [
            "piqueld 0.1.0 started",
            "piqueld 0.1.0 started",
            "piqueld 0.2.0 started (previously 0.1.0)",
        ]
    );
}

/// A roll-back notifies as `daemon_rolled_back`, naming the backup.
#[tokio::test]
async fn roll_backs_notify() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    store
        .record_daemon_rolled_back("pre-7-1000", "piqueld rolled back to pre-7-1000")
        .await
        .expect("the roll-back is recorded");
    store
        .process_notifications()
        .await
        .expect("notifications are queued");
    let deliveries = store
        .deliveries(None, 10)
        .await
        .expect("deliveries list")
        .items;
    let [delivery] = deliveries.as_slice() else {
        panic!("one notification is queued: {deliveries:?}");
    };
    assert_eq!(delivery.category, NotificationCategory::DaemonRolledBack);
    let event = store
        .event(delivery.event_id)
        .await
        .expect("its event reads");
    assert_eq!(event.resource.as_deref(), Some("pre-7-1000"));
}

/// On the Unix socket, refusals by the host operator and by anonymous
/// callers share no account and no address, yet count separately.
#[tokio::test]
async fn operator_and_anonymous_refusals_burst_separately() {
    let (_temp, store) = Store::temporary().await;
    let socket = |uid: Option<u32>| {
        let mut event = refused("unused");
        event.origin.peer = None;
        event.origin.operator = uid.map(|uid| piqueld_core::auth::HostOperator { uid });
        event
    };
    let bursts = async || {
        let messages = "SELECT message FROM events WHERE kind='access_denial_burst' ORDER BY id";
        sqlx::query_scalar::<_, String>(messages)
            .fetch_all(&store.pool)
            .await
            .expect("the rows load")
    };
    for uid in std::iter::repeat_n(None, 19).chain([Some(0)]) {
        store
            .record_audit(&socket(uid))
            .await
            .expect("the store can record audit");
    }
    assert_eq!(bursts().await, Vec::<String>::new());
    for uid in std::iter::once(None).chain(std::iter::repeat_n(Some(0), 19)) {
        store
            .record_audit(&socket(uid))
            .await
            .expect("the store can record audit");
    }
    let messages = bursts().await;
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].contains("an anonymous caller"), "{messages:?}");
    assert!(
        messages[1].contains("host operator (uid 0)"),
        "{messages:?}"
    );
}

/// Pruning stops at the oldest retained record: one recorded after the
/// clock stepped back keeps it, rather than taking it along.
#[tokio::test]
async fn audit_pruning_keeps_records_after_a_retained_one() {
    let (_temp, mut store) = Store::temporary().await;
    store.audit_days = 1;
    for (peer, at) in [("a", 0), ("b", now_ms()), ("c", 0)] {
        store
            .record_audit_at(&refused(peer), at)
            .await
            .expect("the store can record audit at");
    }
    store
        .prune_audit()
        .await
        .expect("the store can prune audit");
    let kept = sqlx::query_scalar::<_, String>("SELECT peer FROM audit_events ORDER BY id")
        .fetch_all(&store.pool)
        .await
        .expect("the rows load");
    assert_eq!(kept, ["b", "c"]);
    let verified = store
        .verify_audit()
        .await
        .expect("the store can verify audit");
    assert_eq!((verified.checked, verified.broken_at), (2, None));
}

/// The audit chain detects edited, renumbered, inserted, and removed records,
/// and pruning keeps it verifiable without ever erasing a broken part.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn the_audit_chain_detects_alteration_and_survives_pruning() {
    let (_temp, mut store) = Store::temporary().await;
    store.audit_days = 1;
    let now = now_ms();
    for (peer, at) in [("a", 0), ("b", 0), ("c", now), ("d", now)] {
        let mut event = refused(peer);
        event.origin.tailnet = (peer == "d").then(|| "tag:ci on runner".into());
        event.origin.operator =
            (peer == "d").then_some(piqueld_core::auth::HostOperator { uid: 1000 });
        store
            .record_audit_at(&event, at)
            .await
            .expect("the store can record audit at");
    }
    let intact = store
        .verify_audit()
        .await
        .expect("the store can verify audit");
    assert_eq!((intact.checked, intact.broken_at), (4, None));
    let renamed = "UPDATE audit_events SET tailnet='tag:prod on runner' WHERE peer='d'";
    sqlx::query(renamed)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    let d_id: i64 = sqlx::query_scalar("SELECT id FROM audit_events WHERE peer='d'")
        .fetch_one(&store.pool)
        .await
        .expect("the row is present");
    assert_eq!(
        store
            .verify_audit()
            .await
            .expect("the store can verify audit")
            .broken_at,
        Some(d_id)
    );
    let restore = "UPDATE audit_events SET tailnet='tag:ci on runner' WHERE peer='d'";
    sqlx::query(restore)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    let operator = "UPDATE audit_events SET operator_uid=0 WHERE peer='d'";
    sqlx::query(operator)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    assert_eq!(
        store
            .verify_audit()
            .await
            .expect("the store can verify audit")
            .broken_at,
        Some(d_id)
    );
    let restore = "UPDATE audit_events SET operator_uid=1000 WHERE peer='d'";
    sqlx::query(restore)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    let run = async |sql: &str| {
        sqlx::query(sql)
            .execute(&store.pool)
            .await
            .expect("the statement executes");
        store
            .verify_audit()
            .await
            .expect("the store can verify audit")
    };
    let edited = run("UPDATE audit_events SET status=200 WHERE peer='b'").await;
    assert_eq!(edited.broken_at, Some(2));
    store
        .prune_audit()
        .await
        .expect("the store can prune audit");
    assert_eq!(
        store
            .verify_audit()
            .await
            .expect("the store can verify audit"),
        edited
    );
    run("UPDATE audit_events SET status=404 WHERE peer='b'").await;
    store
        .prune_audit()
        .await
        .expect("the store can prune audit");
    let pruned = store
        .verify_audit()
        .await
        .expect("the store can verify audit");
    assert_eq!((pruned.checked, pruned.broken_at), (2, None));
    assert_eq!(pruned.anchor.expect("the store can anchor").id, 2);
    assert_eq!(pruned.head, intact.head);
    let edited = run("UPDATE audit_events SET environment_id='env-other' WHERE peer='c'").await;
    assert_eq!(edited.broken_at, Some(3));
    run("UPDATE audit_events SET environment_id=NULL WHERE peer='c'").await;
    let renumbered = run("UPDATE audit_events SET id=9223372036854775807 WHERE peer='d'").await;
    assert_eq!(renumbered.broken_at, Some(i64::MAX));
    assert!(matches!(
        store.record_audit(&refused("e")).await,
        Err(StoreError::Corrupt)
    ));
    run("UPDATE audit_events SET id=4 WHERE peer='d'").await;
    for id in [-5, i64::MIN] {
        let forged = format!(
            "INSERT INTO audit_events(id,created_at_ms,action,outcome,status) \
            VALUES({id},0,'forged','allowed',200)"
        );
        assert_eq!(run(&forged).await.broken_at, Some(id));
        store
            .prune_audit()
            .await
            .expect("the store can prune audit");
        assert_eq!(
            store
                .verify_audit()
                .await
                .expect("the store can verify audit")
                .broken_at,
            Some(id)
        );
        run(&format!("DELETE FROM audit_events WHERE id={id}")).await;
    }
    let half = sqlx::query("UPDATE audit_chain SET pruned_link=NULL");
    half.execute(&store.pool)
        .await
        .expect_err("`execute` should be rejected");
    let removed = run("DELETE FROM audit_events WHERE peer='c'").await;
    assert_eq!(
        removed.broken_at,
        Some(4),
        "removing breaks the next record"
    );
}
/// A record appended inside a write transaction commits or rolls back with
/// it: a rolled-back append leaves neither a row nor a gap in the IDs, and
/// appends interleaved with background records and pruning keep the chain
/// verifiable.
#[tokio::test]
async fn audit_appends_commit_and_roll_back_with_their_transaction() {
    let (_temp, mut store) = Store::temporary().await;
    store.audit_days = 1;
    let now = now_ms();
    let append = async |peer: &str, at: i64, commit: bool| {
        let mut tx = store
            .begin_immediate()
            .await
            .expect("the store can begin immediate");
        Store::append_audit_on(&mut tx, &refused(peer), at)
            .await
            .expect("the store can append audit on");
        if commit {
            tx.commit().await.expect("the transaction commits");
        }
    };
    append("a", 0, false).await;
    store
        .record_audit_at(&refused("b"), 0)
        .await
        .expect("the store can record audit at");
    append("c", now, true).await;
    append("d", now, false).await;
    store
        .record_audit_at(&refused("e"), now)
        .await
        .expect("the store can record audit at");
    let rows = "SELECT id,peer FROM audit_events ORDER BY id";
    let kept = sqlx::query_as::<_, (i64, String)>(rows)
        .fetch_all(&store.pool)
        .await
        .expect("the rows load");
    assert_eq!(kept, [(1, "b".into()), (2, "c".into()), (3, "e".into())]);
    store
        .prune_audit()
        .await
        .expect("the store can prune audit");
    let verified = store
        .verify_audit()
        .await
        .expect("the store can verify audit");
    assert_eq!((verified.checked, verified.broken_at), (2, None));
    assert_eq!(verified.anchor.expect("the store can anchor").id, 1);
}
/// Successful deployments notify only destinations receiving them, naming
/// the application and environment as of then.
#[tokio::test]
async fn successful_deployments_notify_with_their_names() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let mut deployments = settings();
    deployments.categories = [(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    )]
    .into();
    let url = "https://example.com/deployments";
    store.add_destination("deployments", url, deployments).await;
    let op = named_application(&store, "shop").await;
    succeed(&store, &op).await;
    let (delivery, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(delivery.destination, "deployments");
    assert_eq!(delivery.category, NotificationCategory::DeploymentSucceeded);
    assert_eq!(
        delivery.subject.describe().as_deref(),
        Some("shop / production")
    );
    store
        .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
        .await
        .expect("the store can complete delivery");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );

    let mut both = settings();
    both.categories.retain(|category, _| {
        matches!(
            category,
            NotificationCategory::DeploymentFailures | NotificationCategory::Recovery
        )
    });
    both.categories.insert(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    );
    store.add_destination("deployments", url, both).await;
    rerun(&store, &op).await;
    fail(&store, &op).await;
    drain(&store).await;
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    let mut delivered = drain(&store).await;
    delivered.sort();
    let recovery = NotificationCategory::Recovery;
    let expected = [
        ("deployments".to_owned(), recovery),
        ("test".to_owned(), recovery),
    ];
    assert_eq!(delivered, expected);
}
/// A deployment waits for the recovery it brought, and is sent when that
/// recovery is cancelled because the failure was never acknowledged; failed,
/// it can be retried like any occurrence.
#[tokio::test]
async fn deployments_follow_cancelled_recoveries_and_stay_retryable() {
    let (_temp, store) = Store::temporary().await;
    let mut both = settings();
    both.categories.insert(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    );
    store
        .add_destination("test", "https://example.com/private-token", both)
        .await;
    let op = application(&store).await;
    fail(&store, &op).await;
    rerun(&store, &op).await;
    succeed(&store, &op).await;
    let (failure, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(failure.category, NotificationCategory::DeploymentFailures);
    store
        .complete_delivery(
            &failure.id,
            DeliveryState::Pending,
            Some("Receiver down"),
            3600,
        )
        .await
        .expect("the store can complete delivery");
    assert!(
        store
            .claim_delivery()
            .await
            .expect("the delivery claims")
            .is_none()
    );
    sqlx::query("UPDATE notification_deliveries SET retry_started_at_ms=0 WHERE id=?1")
        .bind(&failure.id)
        .execute(&store.pool)
        .await
        .expect("the statement executes");
    let (deployed, _) = store
        .claim_delivery()
        .await
        .expect("the delivery claims")
        .expect("a delivery is pending");
    assert_eq!(deployed.category, NotificationCategory::DeploymentSucceeded);
    store
        .complete_delivery(&deployed.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .expect("the store can complete delivery");
    store
        .retry_delivery(Actor::Daemon, &deployed.id)
        .await
        .expect("the store can retry delivery");
}

#[tokio::test]
async fn migration_types_the_phases_recorded_before_them() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let pool = Store::migrated(&temp.path().join("db"), 1).await;
    for (kind, phase) in [
        ("application_edited", "jobs"),
        ("action_started", "running_job"),
        ("operation_failed", "preparing_sources"),
        ("action_started", "deploy"),
        ("action_started", "ensure_service"),
    ] {
        sqlx::query("INSERT INTO events(scope,kind,phase,created_at_ms) VALUES('daemon',?1,?2,1)")
            .bind(kind)
            .bind(phase)
            .execute(&pool)
            .await
            .expect("the statement executes");
    }
    for (attempt, phase) in [(1, "running_job"), (2, "preparing_sources"), (3, "deploy")] {
        sqlx::raw_sql(&format!(
            "PRAGMA foreign_keys = OFF; INSERT INTO deployment_attempts(deployment_id,attempt,outcome_json) \
             VALUES('operation-1',{attempt},'{{\"phase\":\"{phase}\"}}')"
        ))
        .execute(&pool)
        .await
        .expect("the statement executes");
    }
    pool.close().await;
    let store = Store::open_unsynced(temp.path().join("db"))
        .await
        .expect("the store opens");
    let attempts: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT json_extract(outcome_json,'$.phase') FROM deployment_attempts ORDER BY attempt",
    )
    .fetch_all(&store.pool)
    .await
    .expect("the rows load");
    assert_eq!(
        attempts,
        [Some("run_job".into()), Some("preparing".into()), None]
    );
    let events = store
        .filtered_events(&EventFilter::default(), &Visibility::ALL, None, 10)
        .await
        .expect("the store can filtered events")
        .items;
    let recorded: Vec<_> = events
        .iter()
        .map(|event| (event.phase, event.detail.clone()))
        .collect();
    assert_eq!(
        recorded,
        [
            (
                None,
                Some(piqueld_core::EventDetail::Edited {
                    field: piqueld_core::edit::EditedField::Jobs
                })
            ),
            (Some(Phase::RunJob), None),
            (Some(Phase::Preparing), None),
            (None, None),
            (Some(Phase::EnsureService), None),
        ]
    );
}

#[tokio::test]
async fn timeline_expectations_count_each_earlier_operations_last_run_of_a_step() {
    let (_temp, store) = Store::temporary().await;
    let op = application(&store).await;
    for (attempt, phase, duration) in [
        (1, Phase::BuildGit, 90_000),
        (1, Phase::RunJob, 4_000),
        (2, Phase::BuildGit, 1_000),
    ] {
        let action = store
            .begin_action(Some(&op.id), phase, Some("web"))
            .await
            .expect("the store can begin action");
        store
            .finish_action(&action, None)
            .await
            .expect("the store can finish action");
        sqlx::query("UPDATE events SET attempt=?1, duration_ms=?2 WHERE action_id=?3")
            .bind(attempt)
            .bind(duration)
            .bind(&action.id)
            .execute(&store.pool)
            .await
            .expect("the statement executes");
    }
    sqlx::query(
        "UPDATE operations SET state='succeeded', attempt=2, finished_at_ms=created_at_ms WHERE id=?1",
    )
    .bind(&op.id)
    .execute(&store.pool)
    .await
    .expect("the statement executes");
    let expected = async |operation: Operation| {
        let baseline = store
            .timeline_baseline(&operation)
            .await
            .expect("the store can timeline baseline");
        baseline
            .expected
            .iter()
            .map(|step| (step.key.phase, step.duration_ms))
            .collect::<Vec<_>>()
    };
    let next = Operation {
        id: "operation-next".into(),
        ..op.clone()
    };
    assert_eq!(
        expected(next).await,
        [(Phase::BuildGit, 1_000), (Phase::RunJob, 4_000)]
    );
    let earlier = Operation {
        id: "operation-earlier".into(),
        created_at_ms: op.created_at_ms - 1,
        ..op
    };
    assert_eq!(expected(earlier).await, []);
}

/// A failed operation names its failed action, never a request that was
/// retried and then succeeded: a plan blocked after one names what blocks it.
#[tokio::test]
async fn failed_operations_name_their_failure_not_an_earlier_retry() {
    use piqueld_core::observability::DiagnosticCode;
    use piqueld_core::timeline::PlanCoverage;
    let (_directory, store) = Store::temporary().await;
    let operation = application(&store).await;
    let action = store
        .begin_action(Some(&operation.id), Phase::EnsureService, Some("web"))
        .await
        .expect("the store can begin action");
    store
        .action_request(&action, 1)
        .await
        .expect("the store can action request");
    let busy = Diagnostic::new(
        "diagnostic-1".into(),
        DiagnosticCode::DockerUnavailable,
        "Docker is unavailable".into(),
    );
    store
        .action_retry(&action, 1, Duration::from_secs(1), &busy)
        .await
        .expect("the store can action retry");
    store
        .finish_action(&action, None)
        .await
        .expect("the store can finish action");
    store
        .record_plan(
            &operation.id,
            Vec::new(),
            PlanCoverage::Runtime,
            Some("data"),
        )
        .await
        .expect("the store can record plan");
    store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("ownership_conflict", "a volume is not ours")),
        )
        .await
        .expect("the store can transition operation");
    let failed = store
        .operation(&operation.id)
        .await
        .expect("the store can operation");
    assert_eq!(
        (failed.phase, failed.resource.as_deref()),
        (Some(Phase::Planning), Some("data"))
    );
    let cleanup = store
        .begin_action(Some(&operation.id), Phase::RemoveJobs, None)
        .await
        .expect("the store can begin action");
    store
        .finish_action(&cleanup, Some(busy))
        .await
        .expect("the store can finish action");
    let failed = store
        .operation(&operation.id)
        .await
        .expect("the store can operation");
    assert_eq!(
        (failed.phase, failed.resource.as_deref()),
        (Some(Phase::Planning), Some("data"))
    );
}
/// Requested notification changes commit with their audit record, which
/// names the destination and what it now receives but never its URL: a
/// failed append keeps the previous destination, its URL and the failed
/// delivery.
#[tokio::test]
async fn requested_notification_changes_commit_with_their_audit_record() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    let application = application_of(&store, &op).await;
    fail(&store, &op).await;
    let (failure, _) = store
        .claim_delivery()
        .await
        .expect("deliveries are claimed")
        .expect("the failure is due for delivery");
    let failed = Some("Receiver rejected alert");
    store
        .complete_delivery(&failure.id, DeliveryState::Failed, failed, 3600)
        .await
        .expect("the delivery fails");
    let request = testing::audit_request("POST /api/v1/notifications");
    let author = Author {
        actor: Actor::Daemon,
        request: Some(&request),
    };
    let ops = DestinationName::parse("ops").expect("the name is valid");
    let put = async |url: &str, narrowed: bool| {
        let mut settings = settings();
        if narrowed {
            let only = [application.clone()].into();
            (settings.applications, settings.previews) = (ApplicationSelection::Only(only), false);
        }
        let url = Some(url.into());
        let request = DestinationRequest { url, settings };
        store
            .put_notification_destination(author, &ops, request)
            .await
    };
    let first = "https://example.com/private-token";
    let second = "https://example.com/other-token";
    put(first, false).await.expect("the destination is created");
    let id: String =
        sqlx::query_scalar("SELECT id FROM notification_destinations WHERE name='ops'")
            .fetch_one(&store.pool)
            .await
            .expect("the destination has an ID");
    store.refuse_audit(true).await;
    put(second, true)
        .await
        .expect_err("`put` should be rejected");
    let deleted = store.delete_notification_destination(author, &ops);
    assert!(deleted.await.is_err());
    assert!(store.retry_delivery(author, &failure.id).await.is_err());
    store.refuse_audit(false).await;
    let destination = store
        .notification_destination(&ops)
        .await
        .expect("the destination is read");
    assert_eq!(
        store
            .destination_url(&destination)
            .expect("its URL decrypts")
            .as_str(),
        first
    );
    assert!(
        store
            .claim_delivery()
            .await
            .expect("deliveries are claimed")
            .is_none()
    );
    put(second, true)
        .await
        .expect("the destination is replaced");
    store
        .retry_delivery(author, &failure.id)
        .await
        .expect("the failed delivery is retried");
    store
        .delete_notification_destination(author, &ops)
        .await
        .expect("the destination is deleted");
    let route = "POST /api/v1/notifications";
    let receives = "categories build_failures,deployment_failures,service_degradation,\
        daemon_failures,recovery,daemon_started,daemon_rolled_back; applications";
    let actions = store.audit_actions().await;
    assert_eq!(
        actions,
        [
            format!("{route} destination \"ops\" {id} created: {receives} all; previews yes"),
            format!(
                "{route} destination \"ops\" {id} changed as a new receiver: \
                {receives} {application}; previews no"
            ),
            format!("{route} delivery {} retry queued", failure.id),
            format!("{route} destination \"ops\" {id} deleted"),
        ]
    );
}

/// A requested webhook secret rotation commits with its audit record, which
/// never holds the secret: a failed append keeps the previous secret.
#[tokio::test]
async fn requested_webhook_secret_rotations_commit_with_their_audit_record() {
    let (_temp, store) = Store::temporary().await;
    let op = application(&store).await;
    let application = application_of(&store, &op).await;
    let route = "POST /api/v1/applications/{id}/webhook/secret";
    let request = testing::audit_request(route);
    let author = Author {
        actor: Actor::Daemon,
        request: Some(&request),
    };
    let rotate = async || {
        let (secret, _) = store.generate_webhook_secret(author, &application).await?;
        Ok::<_, StoreError>(secret)
    };
    let kept = rotate().await.expect("the webhook secret is generated");
    store.refuse_audit(true).await;
    rotate().await.expect_err("`rotate` should be rejected");
    store.refuse_audit(false).await;
    let webhook = store
        .webhook_secret(&application)
        .await
        .expect("the webhook secret is read")
        .expect("the application has a webhook secret");
    assert_eq!(webhook.as_slice(), kept.as_bytes());
    let secret = rotate().await.expect("the webhook secret is rotated");
    let actions = store.audit_actions().await;
    let generated = format!("{route} webhook secret generated");
    assert_eq!(actions, [generated.clone(), generated]);
    assert!(
        !actions
            .iter()
            .any(|action| { action.contains(kept.as_str()) || action.contains(secret.as_str()) })
    );
}

/// Statistics count the writes that waited for their turn and how long, and
/// how long the oldest queued operation, which a deploy queues once the held
/// turn is released, has waited.
#[tokio::test]
async fn daemon_stats_report_write_waits_and_queue_ages() {
    let (_temp, store) = Store::temporary().await;
    let writer = store.hold_writer().await;
    let release = async {
        store.writer_waiting().await;
        wait::pause(Duration::from_millis(20)).await;
        drop(writer);
    };
    let (saved, ()) = tokio::join!(
        store.save_template(template("queued", "[spec]"), true),
        release
    );
    assert!(saved.operation_id.is_some());
    let stats = store.daemon_stats().await.expect("the daemon stats load");
    assert_eq!((stats.waiting_writes, stats.write_waits), (0, 1));
    assert!(stats.write_wait_ms >= 20, "{stats:?}");
    assert!(
        stats.oldest_queued_operation_ms.is_some_and(|age| age >= 0),
        "{stats:?}"
    );
    assert_eq!(stats.oldest_pending_delivery_ms, None);
}

/// A retried operation has waited since its retry, not since it was first
/// created.
#[tokio::test]
async fn a_retried_operation_waits_from_its_retry() {
    let (_temp, store) = Store::temporary().await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let three_days_ago = now_ms() - 3 * 86_400_000;
    sqlx::query("UPDATE operations SET created_at_ms=?1,updated_at_ms=?1 WHERE id=?2")
        .bind(three_days_ago)
        .bind(&op.id)
        .execute(&store.pool)
        .await
        .expect("the operation is backdated");
    let failed = store
        .operation(&op.id)
        .await
        .expect("the failed operation loads");
    store
        .retry_operation(&failed)
        .await
        .expect("the operation is retried");
    let stats = store.daemon_stats().await.expect("the daemon stats load");
    assert!(
        stats
            .oldest_queued_operation_ms
            .is_some_and(|age| (0..60_000).contains(&age)),
        "{stats:?}"
    );
}
