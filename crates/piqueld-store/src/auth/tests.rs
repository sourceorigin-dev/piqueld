use super::*;
use crate::testing::{credential, user};
use std::time::Duration;

/// Opens a store whose admin `alice` holds the browser session
/// `credential`, with secret hash `hash`.
async fn browser_session() -> (tempfile::TempDir, Store) {
    let (dir, store) = Store::temporary().await;
    store.seed_auth_user(&user("alice"), &Grants::admin()).await;
    let credential = NewCredential {
        secret_hash: "hash".into(),
        name: "Browser",
        expires_at: Some(now_secs() + SESSION_IDLE_SECS),
        ..credential("credential", CredentialKind::Browser)
    };
    store.insert_credential("alice", &credential).await.unwrap();
    (dir, store)
}

/// Authentication reads never wait for writers; refreshes queue behind
/// them without holding a pool connection, and a revocation that lands
/// first wins.
#[tokio::test]
async fn credential_reads_skip_the_writer_queue_and_refreshes_respect_revocation() {
    let (_dir, store) = browser_session().await;

    let (writer, tx) = store.begin_immediate().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), store.credential_owner("hash"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let refresh = store.touch_credential("credential");
    tokio::pin!(refresh);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut refresh)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    // The queued refresh holds no pool connection while it waits.
    let mut connections = Vec::new();
    for _ in 0..8 {
        connections.push(
            tokio::time::timeout(Duration::from_secs(1), store.pool.acquire())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // Revoke while the refresh is still queued behind the writer.
    sqlx::query!("DELETE FROM auth_credentials")
        .execute(&mut *connections[0])
        .await
        .unwrap();
    drop(connections);
    drop(writer);
    assert!(!refresh.await.unwrap());
    assert!(store.credential_owner("hash").await.unwrap().is_none());
}

/// A refresh judges idle expiry when it gets the writer lock, so a session
/// that expires while the refresh waits is not revived.
#[tokio::test]
async fn refreshes_waiting_for_writers_respect_idle_expiry() {
    let (_dir, store) = browser_session().await;
    // One second of idle time remains, and passes while the refresh waits.
    store.age_auth_credentials(SESSION_IDLE_SECS - 1).await;
    let (writer, tx) = store.begin_immediate().await.unwrap();
    let refresh = tokio::spawn({
        let store = store.clone();
        async move { store.touch_credential("credential").await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    tx.rollback().await.unwrap();
    drop(writer);
    assert!(!refresh.await.unwrap().unwrap());
}

/// A credential is fresh for two minutes after a passkey proved its
/// holder present, inclusive; a missing or future time is stale, and an
/// API token never is fresh.
#[test]
fn freshness_needs_a_recent_passkey_proof() {
    let now = 1_000_000;
    let stale = Err(Denied::Reauthenticate(Reauthenticate::Stale));
    let oldest = now - Reauthenticate::WINDOW_SECS;
    for (authenticated_at, expected) in [
        (Some(now), Ok(now)),
        (Some(oldest), Ok(oldest)),
        (Some(now - Reauthenticate::WINDOW_SECS - 1), stale),
        (Some(now + 1), stale),
        (None, stale),
    ] {
        for kind in [CredentialKind::Browser, CredentialKind::Cli] {
            let proof = Proof {
                kind,
                authenticated_at,
            };
            assert_eq!(proof.require_fresh(now), expected, "{proof:?}");
        }
    }
    let token = Proof {
        kind: CredentialKind::Token,
        authenticated_at: Some(now),
    };
    assert_eq!(
        token.require_fresh(now),
        Err(Denied::Reauthenticate(Reauthenticate::Token))
    );
}

/// The store reads a credential's stored proof time both when
/// authenticating and inside writes, ordinary use never extends it, and
/// the host operator is exempt from freshness.
#[tokio::test]
async fn writes_check_the_stored_proof_and_exempt_the_host_operator() {
    let (_dir, store) = browser_session().await;
    let proved = now_secs();
    for (id, kind, authenticated_at) in [
        ("fresh", CredentialKind::Browser, Some(proved)),
        ("token", CredentialKind::Token, Some(proved)),
    ] {
        let credential = NewCredential {
            authenticated_at,
            ..credential(id, kind)
        };
        store.insert_credential("alice", &credential).await.unwrap();
    }
    let owner = store.credential_owner("fresh").await.unwrap().unwrap();
    assert_eq!(owner.proof.authenticated_at, Some(proved));
    store.age_auth_credentials(3600).await;
    assert!(store.touch_credential("fresh").await.unwrap());
    let owner = store.credential_owner("fresh").await.unwrap().unwrap();
    assert_eq!(owner.proof.authenticated_at, Some(proved));

    let account = |credential_id| {
        Actor::Account(Caller {
            credential_id,
            user_id: "alice",
        })
    };
    let mut db = store.connection().await.unwrap();
    account("fresh").require_fresh(&mut db).await.unwrap();
    for (id, reason) in [
        // Signed in by `browser_session` without a passkey proof.
        ("credential", Reauthenticate::Stale),
        ("token", Reauthenticate::Token),
    ] {
        assert!(matches!(
            account(id).require_fresh(&mut db).await,
            Err(StoreError::Denied(Denied::Reauthenticate(refused))) if refused == reason
        ));
    }
    let operator = HostOperator { uid: 0 };
    let socket = Actor::Operator {
        operator,
        session: None,
    };
    socket.require_fresh(&mut db).await.unwrap();
    drop(db);
    let expires_at = now_secs() + 3600;
    store
        .create_operator_link("operator", "link", expires_at, operator)
        .await
        .unwrap();
    let (session, _) = store
        .redeem_operator_link("link", "session", expires_at)
        .await
        .unwrap()
        .unwrap();
    let browser = Actor::Operator {
        operator,
        session: Some(&session),
    };
    let mut db = store.connection().await.unwrap();
    browser.require_fresh(&mut db).await.unwrap();
}

/// Pruning deletes expired credentials, idle browser sessions, and
/// expired invitations, and keeps what can still be used.
#[tokio::test]
async fn pruning_deletes_unusable_auth_state() {
    let (_dir, store) = Store::temporary().await;
    store.seed_auth_user(&user("alice"), &Grants::admin()).await;
    let now = now_secs();
    for (id, kind, expires_at) in [
        ("expired", CredentialKind::Token, Some(now)),
        ("idle", CredentialKind::Browser, Some(now + 3600)),
        ("live", CredentialKind::Token, None),
    ] {
        let credential = NewCredential {
            expires_at,
            ..credential(id, kind)
        };
        store.insert_credential("alice", &credential).await.unwrap();
    }
    store.age_auth_credentials(SESSION_IDLE_SECS).await;
    for (id, expires_at) in [("old", now), ("open", now + 3600)] {
        sqlx::query!(
            "INSERT INTO auth_invitations(id,issuer_id,secret_hash,expires_at) VALUES(?1,'alice',?1,?2)",
            id,
            expires_at
        )
        .execute(&store.pool)
        .await
        .unwrap();
    }

    store.prune_auth().await.unwrap();
    let credentials = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_credentials"#)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    assert_eq!(credentials, ["live"]);
    let invitations = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_invitations"#)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    assert_eq!(invitations, ["open"]);
}

/// A CLI login keeps the latest of its confirmations' proofs, whichever
/// confirmation finishes last.
#[tokio::test]
async fn confirming_a_login_keeps_its_latest_proof() {
    let (_dir, store) = Store::temporary().await;
    let Actor::Account(alice) = store.seed_caller("alice", &Grants::admin()).await else {
        unreachable!("seeded an account caller")
    };
    let laptop = credential("laptop", CredentialKind::Cli);
    let inserted = store.insert_credential("alice", &laptop).await;
    inserted.expect("the store keeps the login");
    let now = now_secs();
    // The newer confirmation finishes first.
    for proved in [now - 10, now - 50] {
        let confirmed = store.confirm_login(alice, "laptop", proved).await;
        assert!(confirmed.expect("alice may confirm her own login"));
    }
    let owner = store.credential_owner("laptop").await;
    let owner = owner.expect("credentials are readable");
    let owner = owner.expect("the login is live");
    assert_eq!(owner.proof.authenticated_at, Some(now - 10));
}
