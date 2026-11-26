//! Daemon settings: changes apply at once, and each refusal has its code.
use super::*;

/// A setting changes and resets over the API, and the store applies it at
/// once; unknown, pinned and out-of-range changes are refused with their
/// status and code.
#[tokio::test]
async fn settings_change_and_reset_unless_unknown_pinned_or_out_of_range() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let store = Arc::new(
        Store::open_unsynced(database(&temp))
            .await
            .expect("the store opens"),
    );
    let mut config = DaemonConfig::default();
    config.retention.audit_days = Some(30);
    let (_runtime, state) = contract_state(&temp, Arc::clone(&store), &config).await;
    let server = Server::tcp(router(state, FakeAuth)).await;
    let client = server.client();

    let changed = client
        .put_setting("retention.event_days", serde_json::json!(7))
        .await
        .expect("the setting is changed");
    assert_eq!(changed.value, serde_json::json!(7));
    assert_eq!(store.settings().retention.event_days, 7);
    let reset = client
        .reset_setting("retention.event_days")
        .await
        .expect("the setting is reset");
    assert_eq!(reset.value, serde_json::json!(90));
    assert_eq!(store.settings().retention.event_days, 90);

    for (key, value, status, code) in [
        (
            "retention.forever",
            serde_json::json!(1),
            404,
            "setting_unknown",
        ),
        (
            "retention.audit_days",
            serde_json::json!(1),
            409,
            "setting_pinned",
        ),
        (
            "build.unavailable",
            serde_json::json!("local"),
            409,
            "setting_unavailable",
        ),
        (
            "reconciliation.scan_interval_seconds",
            serde_json::json!(0),
            422,
            "setting_invalid",
        ),
    ] {
        let error = api_error(
            client
                .put_setting(key, value)
                .await
                .expect_err("the change is refused"),
        );
        assert_eq!((error.status, error.code.as_str()), (status, code), "{key}");
    }
    assert_eq!(store.settings().retention.audit_days, 30);
}
