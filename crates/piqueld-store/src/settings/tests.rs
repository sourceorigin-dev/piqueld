use crate::{Actor, Author, Store, StoreError};
use piqueld_core::access::{GlobalPermission, Grants, Permission, Scope};
use piqueld_core::api::SettingSource;
use piqueld_core::settings::SettingError;
use serde_json::json;

/// A set value applies at once, is recorded in the audit trail, survives a
/// restart, and a reset deletes it.
#[tokio::test]
async fn set_settings_apply_at_once_and_survive_a_restart() {
    let (directory, store) = Store::temporary().await;
    let route = "PUT /api/v1/system/settings/{key}";
    let request = crate::testing::audit_request(route);
    let author = Author {
        actor: Actor::Daemon,
        request: Some(&request),
    };
    let set = store
        .set_setting(author, "retention.event_days", Some(json!(7)))
        .await
        .expect("the setting is changed");
    assert_eq!((set.value, set.source), (json!(7), SettingSource::Changed));
    assert_eq!(store.settings().retention.event_days, 7);
    for (key, value) in [
        ("retention.event_days", Some(json!("7"))),
        ("reconciliation.scan_interval_seconds", Some(json!(0))),
    ] {
        let refused = store.set_setting(author, key, value).await;
        assert!(
            matches!(
                refused,
                Err(StoreError::Setting(SettingError::Invalid { .. }))
            ),
            "{refused:?}"
        );
    }
    let unknown = store.set_setting(author, "retention.forever", None).await;
    assert!(matches!(
        unknown,
        Err(StoreError::Setting(SettingError::Unknown(_)))
    ));
    drop(store);

    let store = Store::open_unsynced(directory.path().join("db"))
        .await
        .expect("the store opens");
    assert_eq!(store.settings().retention.event_days, 7);
    let reset = store
        .set_setting(author, "retention.event_days", None)
        .await
        .expect("the setting is reset");
    assert_eq!(
        (reset.value, reset.source),
        (json!(90), SettingSource::Default)
    );
    assert_eq!(store.settings().retention.event_days, 90);
    assert_eq!(
        store.audit_actions().await,
        [
            format!("{route} setting retention.event_days changed from 90 to 7"),
            format!("{route} setting retention.event_days reset from 7 to its default 90"),
        ]
    );
}

/// Only callers holding `settings:manage` change settings.
#[tokio::test]
async fn changing_a_setting_needs_settings_manage() {
    let (_directory, store) = Store::temporary().await;
    let mut grants = Grants::default();
    grants
        .grant(
            Permission::Global(GlobalPermission::SystemRead),
            &Scope::All,
        )
        .expect("the permission is granted");
    let reader = store.seed_caller("reader", &grants).await;
    let refused = store
        .set_setting(reader, "previews.max_total", Some(json!(1)))
        .await;
    assert!(matches!(refused, Err(StoreError::Denied(_))), "{refused:?}");
    assert_eq!(store.settings().previews.max_total, 30);

    grants
        .grant(
            Permission::Global(GlobalPermission::SettingsManage),
            &Scope::All,
        )
        .expect("the permission is granted");
    let manager = store.seed_caller("manager", &grants).await;
    store
        .set_setting(manager, "previews.max_total", Some(json!(1)))
        .await
        .expect("the setting is changed");
    assert_eq!(store.preview_limits().max_total, 1);
}
