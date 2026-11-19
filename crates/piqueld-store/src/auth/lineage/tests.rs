use super::*;
use crate::{Actor, Caller, CredentialKind, NewCredential, NewInvitation, NewPasskey};
use crate::{PasskeyOwner, now_secs};
use piqueld_core::access::Grants;
use piqueld_core::auth::User;

/// The IDs of `node`'s issuers, nearest first.
async fn ancestry(store: &Store, node: Issued<'_>) -> Vec<String> {
    let (kind, id) = node.key();
    sqlx::query_scalar!(
        r#"WITH RECURSIVE up(node,depth) AS (
            SELECT issued_by,1 FROM auth_lineage WHERE kind=?1 AND id=?2
            UNION ALL
            SELECT l.issued_by,up.depth+1 FROM auth_lineage l JOIN up ON l.node=up.node
        )
        SELECT l.id AS "id!" FROM up JOIN auth_lineage l ON l.node=up.node ORDER BY depth"#,
        kind,
        id
    )
    .fetch_all(&store.pool)
    .await
    .unwrap()
}

/// A credential with no expiry or limits, named after its ID.
fn credential(id: &str, kind: CredentialKind) -> NewCredential<'_> {
    NewCredential {
        id: id.into(),
        secret_hash: id.into(),
        kind,
        name: id,
        expires_at: None,
        grants: None,
        authenticated_at: None,
    }
}

/// Seeds the administrator `user`, signed in with the login `user`, which
/// adds the passkey `<user>-key`. Returns that login.
async fn admin_with_passkey(store: &Store, user: &'static str) -> Caller<'static> {
    let Actor::Account(caller) = store.seed_caller(user, &Grants::admin()).await else {
        unreachable!("seeded an account caller")
    };
    let key = format!("{user}-key");
    let passkey = NewPasskey {
        id: &key,
        name: &key,
        credential: "{}",
    };
    let own = PasskeyOwner::Existing(caller, user);
    let added = store.add_passkey(own, passkey).await;
    assert!(added.expect("an account adds its own passkey"));
    caller
}

/// Invites `user` through `issuer`'s credential and redeems it, giving
/// `user` the passkey `<user>-key` and the session `<user>-session`.
async fn invite_and_redeem(store: &Store, issuer: Caller<'_>, user: &str) {
    let id = format!("{user}-invitation");
    let invitation = NewInvitation {
        id: id.clone(),
        secret_hash: id.clone(),
        expires_at: now_secs() + 3600,
    };
    let none = Grants::default();
    let created = store.create_invitation(issuer, &invitation, &none).await;
    created.expect("the issuer may invite");
    let account = User {
        id: user.into(),
        username: user.into(),
        display_name: String::new(),
    };
    let (key, session) = (format!("{user}-key"), format!("{user}-session"));
    let redeem = PasskeyOwner::Redeem {
        user: &account,
        invitation_hash: &id,
        session: credential(&session, CredentialKind::Browser),
    };
    let passkey = NewPasskey {
        id: &key,
        name: &key,
        credential: "{}",
    };
    let redeemed = store.add_passkey(redeem, passkey).await;
    assert!(redeemed.expect("the invitation is redeemable"));
}

/// Whether the credential with secret hash `id` still authenticates.
async fn live(store: &Store, id: &str) -> bool {
    let owner = store.credential_owner(id).await;
    owner.expect("credentials are readable").is_some()
}

/// The IDs of every stored passkey, in order.
async fn passkeys(store: &Store) -> Vec<String> {
    sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_passkeys ORDER BY id"#)
        .fetch_all(&store.pool)
        .await
        .expect("passkeys are readable")
}

/// Each issuing path records what issued it, and pruning keeps the
/// chain from a live credential to its root even after sessions in
/// between logged out and the invitation was redeemed, while forgetting
/// nodes nothing live descends from.
#[tokio::test]
async fn lineage_records_issuers_and_survives_vanished_intermediates() {
    let (_dir, store) = Store::temporary().await;
    let alice = store.seed_caller("alice", &Grants::admin()).await;
    let Actor::Account(alice) = alice else {
        unreachable!("seeded an account caller")
    };
    let invitation = NewInvitation {
        id: "invitation".into(),
        secret_hash: "invitation".into(),
        expires_at: now_secs() + 3600,
    };
    store
        .create_invitation(alice, &invitation, &Grants::default())
        .await
        .unwrap();
    let bob = User {
        id: "bob".into(),
        username: "bob".into(),
        display_name: String::new(),
    };
    let passkey = |id| NewPasskey {
        id,
        name: id,
        credential: "{}",
    };
    let redeem = PasskeyOwner::Redeem {
        user: &bob,
        invitation_hash: "invitation",
        session: credential("redeemed", CredentialKind::Browser),
    };
    assert!(store.add_passkey(redeem, passkey("bob-key")).await.unwrap());
    let signed_in = credential("signed-in", CredentialKind::Browser);
    let stored = |stored: &str| Ok::<_, StoreError>(stored.to_owned());
    store
        .sign_in_with_passkey("bob-key", "bob", stored, &signed_in)
        .await
        .unwrap()
        .unwrap();
    let bob_caller = |credential_id| Caller {
        credential_id,
        user_id: "bob",
    };
    let own = PasskeyOwner::Existing(bob_caller("signed-in"), "bob");
    assert!(store.add_passkey(own, passkey("second-key")).await.unwrap());
    let mut token = credential("token", CredentialKind::Token);
    let none = Grants::default();
    token.grants = Some(&none);
    store
        .create_token(bob_caller("signed-in"), &token, None)
        .await
        .unwrap();
    let cli = credential("cli", CredentialKind::Cli);
    store
        .issue_for_credential_owner(bob_caller("redeemed"), &cli)
        .await
        .unwrap()
        .unwrap();
    // A leaf nothing descends from, gone once its credential is.
    let leaf = credential("leaf", CredentialKind::Token);
    store
        .issue_for_credential_owner(alice, &leaf)
        .await
        .unwrap()
        .unwrap();

    // Both of bob's sessions and the leaf log out.
    for id in ["redeemed", "signed-in", "leaf"] {
        let logout = store.revoke_credential(Actor::Daemon, id, Cascade::No);
        logout.await.unwrap();
    }
    store.prune_auth().await.unwrap();

    let chain = ["signed-in", "bob-key", "invitation", "alice"];
    assert_eq!(ancestry(&store, Issued::Credential("token")).await, chain);
    assert_eq!(ancestry(&store, Issued::Passkey("second-key")).await, chain);
    let chain = ["redeemed", "bob-key", "invitation", "alice"];
    assert_eq!(ancestry(&store, Issued::Credential("cli")).await, chain);
    let nodes = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_lineage ORDER BY id"#)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    let kept = [
        "alice",
        "bob-key",
        "cli",
        "invitation",
        "redeemed",
        "second-key",
        "signed-in",
        "token",
    ];
    assert_eq!(nodes, kept);
}

/// Revoking cascades to everything issued through the credential, in
/// other accounts too, even through a session that already logged out,
/// and leaves siblings and accounts alone. It refuses, changing nothing,
/// when removed passkeys would leave no administrator; without cascade,
/// what the credential issued stays valid.
#[tokio::test]
async fn revoking_cascades_through_ended_issuers() {
    let (_dir, store) = Store::temporary().await;
    let alice = admin_with_passkey(&store, "alice").await;
    invite_and_redeem(&store, alice, "bob").await;
    let none = Grants::default();
    let mut token = credential("token", CredentialKind::Token);
    token.grants = Some(&none);
    let bob = Caller {
        credential_id: "bob-session",
        user_id: "bob",
    };
    let created = store.create_token(bob, &token, None).await;
    created.expect("bob's session creates a token");
    let logout = store.revoke_credential(Actor::Daemon, "bob-session", Cascade::No);
    logout.await.expect("bob's session logs out");
    let other = credential("other", CredentialKind::Cli);
    let inserted = store.insert_credential("alice", &other).await;
    inserted.expect("alice gets another login");
    let other = Caller {
        credential_id: "other",
        user_id: "alice",
    };
    let sibling = credential("sibling", CredentialKind::Cli);
    let issued = store.issue_for_credential_owner(other, &sibling).await;
    let issued = issued.expect("the other login issues a sibling");
    issued.expect("the other login is live");

    // Alice's own passkey descends from her login, and nobody else could
    // administer the installation without it.
    let revoke = store.revoke_credential(Actor::Daemon, "alice", Cascade::Yes);
    assert!(matches!(
        revoke.await,
        Err(StoreError::Lockout(crate::Lockout::LastAdmin))
    ));
    assert!(live(&store, "alice").await && live(&store, "token").await);
    assert_eq!(passkeys(&store).await, ["alice-key", "bob-key"]);

    admin_with_passkey(&store, "carol").await;
    let revoke = store.revoke_credential(Actor::Daemon, "alice", Cascade::Yes);
    revoke
        .await
        .expect("carol still administers, so the cascade goes through");
    assert!(!live(&store, "alice").await && !live(&store, "token").await);
    assert_eq!(passkeys(&store).await, ["carol-key"]);
    assert!(live(&store, "other").await && live(&store, "sibling").await);
    let bob = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_users WHERE id='bob'"#)
        .fetch_optional(&store.pool)
        .await
        .expect("accounts are readable");
    assert!(bob.is_some(), "accounts are never deleted");

    let alone = store.revoke_credential(Actor::Daemon, "other", Cascade::No);
    alone.await.expect("the other login is revoked alone");
    assert!(!live(&store, "other").await && live(&store, "sibling").await);
}
