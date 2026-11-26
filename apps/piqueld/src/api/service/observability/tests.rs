use super::*;
use piqueld_store::{CredentialKind, NewCredential, Store};
use sqlx::Connection as _;
use std::sync::Arc;

/// A credential address whose write fails is forgotten, so the next use
/// of that address notes it instead of being skipped for the whole run.
#[tokio::test]
async fn failed_address_notes_are_retried() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let database = temp.path().join("db");
    let store = Arc::new(
        Store::open_unsynced(&database)
            .await
            .expect("the store opens"),
    );
    let user = piqueld_core::auth::User {
        id: "alice".into(),
        username: "alice".into(),
        display_name: String::new(),
    };
    store
        .seed_auth_user(&user, &piqueld_core::access::Grants::admin())
        .await;
    let credential = NewCredential {
        id: "laptop".into(),
        secret_hash: "hash".into(),
        kind: CredentialKind::Cli,
        name: "piquelctl",
        expires_at: None,
        grants: None,
        authenticated_at: None,
    };
    store
        .insert_credential(&user.id, &credential)
        .await
        .expect("insert credential succeeds");
    let service = ApplicationService::unconnected(store, temp.path()).await;
    let url = format!("sqlite:{}", database.display());
    let mut db = sqlx::SqliteConnection::connect(&url)
        .await
        .expect("the connection opens");
    let address = IpAddr::from([192, 0, 2, 1]);
    let drained = std::time::Duration::from_secs(5);
    for rename in [
        "auth_credential_addresses RENAME TO hidden",
        "hidden RENAME TO auth_credential_addresses",
    ] {
        let rename = format!("ALTER TABLE {rename}");
        sqlx::query(&rename)
            .execute(&mut db)
            .await
            .expect("execute succeeds");
        service.observe_address("laptop", "alice", address);
        service.drain_audit(drained).await;
    }
    let noted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_credential_addresses")
        .fetch_one(&mut db)
        .await
        .expect("fetch one succeeds");
    assert_eq!(noted, 1);
}

/// An audit record that could not be written leaves a marker counting it
/// before the next one, so the trail shows the gap, or, when none
/// follows, once the trail is drained at shutdown.
#[tokio::test]
async fn lost_audit_records_leave_a_marker() {
    let temp = tempfile::tempdir().expect("the temp dir is created");
    let database = temp.path().join("db");
    let store = Arc::new(
        Store::open_unsynced(&database)
            .await
            .expect("the store opens"),
    );
    let service = ApplicationService::unconnected(Arc::clone(&store), temp.path()).await;
    let url = format!("sqlite:{}", database.display());
    let mut db = sqlx::SqliteConnection::connect(&url)
        .await
        .expect("the connection opens");
    let drained = std::time::Duration::from_secs(5);
    let (hide, show) = (
        "audit_events RENAME TO hidden",
        "hidden RENAME TO audit_events",
    );
    for (rename, action) in [
        (hide, Some("POST /lost")),
        (show, Some("POST /written")),
        (hide, Some("POST /last")),
        (show, None),
    ] {
        let rename = format!("ALTER TABLE {rename}");
        sqlx::query(&rename)
            .execute(&mut db)
            .await
            .expect("execute succeeds");
        if let Some(action) = action {
            let mut event = NewAuditEvent::lost(0);
            event.action = action.into();
            service.record_audit(event);
        }
        service.drain_audit(drained).await;
    }
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events ORDER BY id")
        .fetch_all(&mut db)
        .await
        .expect("fetch all succeeds");
    let missing = "audit records missing before this one: 1";
    assert_eq!(actions, [missing, "POST /written", missing]);
}

/// A committed change's record never waits in the background backlog:
/// with the backlog full, a change still commits with its record, while
/// a background record is dropped and counted.
#[tokio::test]
async fn committed_changes_record_themselves_with_the_backlog_full() {
    let temp = tempfile::tempdir().expect("a temporary directory is created");
    let database = temp.path().join("db");
    let store = Store::open_unsynced(&database).await;
    let store = Arc::new(store.expect("the store opens"));
    let service = ApplicationService::unconnected(Arc::clone(&store), temp.path()).await;
    let all = u32::try_from(super::super::AUDIT_BACKLOG).expect("the backlog fits a permit count");
    let full = Arc::clone(&service.audit_backlog).try_acquire_many_owned(all);
    let _full = full.expect("the backlog is free");
    let request = piqueld_store::AuditRequest {
        route: "POST /api/v1/applications".into(),
        origin: piqueld_store::AuditOrigin::default(),
        status: None,
        recorded: piqueld_store::Recorded::default(),
    };
    let author = piqueld_store::Author {
        actor: super::super::Actor::Daemon,
        request: Some(&request),
    };
    let template = piqueld_core::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec]",
    );
    let save = super::super::Mutation::save(template.expect("the manifest parses"), None, false);
    let saved = service.accept(author, save, Some(0), false, None).await;
    saved.expect("the application is saved");
    service.record_audit(NewAuditEvent::lost(0));
    assert_eq!(service.audit_lost.load(Ordering::Relaxed), 1);
    let url = format!("sqlite:{}", database.display());
    let mut db = sqlx::SqliteConnection::connect(&url).await;
    let db = db.as_mut().expect("the database opens");
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events")
        .fetch_all(db)
        .await
        .expect("the trail is read");
    assert_eq!(actions, ["POST /api/v1/applications generation 1"]);
}
