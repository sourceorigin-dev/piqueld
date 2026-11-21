use super::*;
use crate::testing::{credential, proved, user};
use crate::{Actor, Caller, CredentialKind, NewInvitation, NewPasskey};
use crate::{PasskeyOwner, now_secs};
use piqueld_core::access::{GlobalPermission, Grants, Permission, Scope};

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
    let account = crate::testing::user(user);
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
    let bob = user("bob");
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
    // Proved by its passkey sign-in, as the daemon records it, so it may add
    // a passkey and create a token.
    let signed_in = proved(credential("signed-in", CredentialKind::Browser));
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
    // Device logins inherit the passkey proof of their approval.
    let cli = proved(credential("cli", CredentialKind::Cli));
    store
        .issue_for_credential_owner(bob_caller("redeemed"), &cli)
        .await
        .unwrap()
        .unwrap();
    // A leaf nothing descends from, gone once its credential is.
    let leaf = proved(credential("leaf", CredentialKind::Token));
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
    let alice = store.admin_with_passkey("alice").await;
    invite_and_redeem(&store, alice, "bob").await;
    let none = Grants::default();
    let mut token = credential("token", CredentialKind::Token);
    token.grants = Some(&none);
    let bob = Caller {
        credential_id: "bob-session",
        user_id: "bob",
    };
    // Bob signs in with his passkey, so his session may create a token.
    let signed_in_at = now_secs();
    sqlx::query!(
        "UPDATE auth_credentials SET authenticated_at=?1 WHERE id='bob-session'",
        signed_in_at
    )
    .execute(&store.pool)
    .await
    .expect("the session is stored");
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
    // Device logins inherit the passkey proof of their approval.
    let sibling = proved(credential("sibling", CredentialKind::Cli));
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

    store.admin_with_passkey("carol").await;
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

/// Authority over the revoked credential's account is enough: a manager who
/// may change alice's account, but not bob's, still revokes what alice's
/// login issued in bob's account.
#[tokio::test]
async fn revoking_reaches_accounts_the_actor_could_not_change() {
    let (_dir, store) = Store::temporary().await;
    // An administrator nothing here touches, so no cascade locks out.
    let erin = store.admin_with_passkey("erin").await;
    let mut manage = Grants::default();
    let accounts = Permission::Global(GlobalPermission::AccountsManage);
    manage
        .grant(accounts, &Scope::All)
        .expect("accounts:manage is a valid grant");
    store.seed_caller("alice", &manage).await;
    let alice = Caller {
        credential_id: "alice",
        user_id: "alice",
    };
    invite_and_redeem(&store, alice, "bob").await;
    let admin = Grants::admin();
    let promote = store.set_user_grants(Actor::Account(erin), "bob", &admin);
    promote.await.expect("erin makes bob an administrator");
    let manager = store.seed_caller("mia", &manage).await;

    let direct = store.revoke_credential(manager, "bob-session", Cascade::No);
    assert!(
        matches!(direct.await, Err(StoreError::Denied(_))),
        "mia may not change bob's account"
    );
    let revoke = store.revoke_credential(manager, "alice", Cascade::Yes);
    revoke.await.expect("mia may change alice's account");
    assert!(!live(&store, "bob-session").await);
    assert_eq!(passkeys(&store).await, ["erin-key"]);
}

/// Revoking all of an account's credentials, removing a passkey, and
/// deleting an account each revoke what was issued through them, unless
/// asked not to. Revoking all starts from every stored credential, even
/// expired ones.
#[tokio::test]
async fn bulk_revocations_cascade_from_every_root() {
    let (_dir, store) = Store::temporary().await;
    // An administrator nothing here touches, so no cascade locks out.
    let erin = store.admin_with_passkey("erin").await;
    let passkey = async |id: &str| passkeys(&store).await.iter().any(|key| key == id);

    // Alice's expired login had invited bob.
    store.seed_caller("alice", &Grants::admin()).await;
    let alice = Caller {
        credential_id: "alice",
        user_id: "alice",
    };
    invite_and_redeem(&store, alice, "bob").await;
    sqlx::query!("UPDATE auth_credentials SET expires_at=1 WHERE id='alice'")
        .execute(&store.pool)
        .await
        .expect("alice's login expires");
    let all = store.revoke_credentials(Actor::Daemon, "alice", Cascade::Yes);
    all.await
        .expect("the daemon revokes all of alice's credentials");
    assert!(!passkey("bob-key").await && !live(&store, "bob-session").await);

    // Removing a passkey reaches the sessions it signed in and their
    // tokens, unless asked not to.
    let stored = |stored: &str| Ok::<_, StoreError>(stored.to_owned());
    let none = Grants::default();
    for (user, cascade, kept) in [("carol", Cascade::No, true), ("grace", Cascade::Yes, false)] {
        invite_and_redeem(&store, erin, user).await;
        let (key, session) = (format!("{user}-key"), format!("{user}-signed-in"));
        let signed_in = proved(credential(&session, CredentialKind::Browser));
        let sign_in = store
            .sign_in_with_passkey(&key, user, stored, &signed_in)
            .await;
        let sign_in = sign_in.expect("the passkey signs in");
        sign_in.expect("the passkey belongs to its user");
        let token_id = format!("{user}-token");
        let mut token = credential(&token_id, CredentialKind::Token);
        token.grants = Some(&none);
        let by_session = Caller {
            credential_id: &session,
            user_id: user,
        };
        let created = store.create_token(by_session, &token, None).await;
        created.expect("the signed-in session creates a token");
        let remove = store.remove_passkey(Actor::Daemon, &key, cascade);
        remove.await.expect("the daemon removes the passkey");
        assert_eq!(live(&store, &session).await, kept, "{cascade:?}");
        assert_eq!(live(&store, &token_id).await, kept, "{cascade:?}");
    }

    // Deleting an account reaches what its credentials issued elsewhere,
    // without deleting those accounts.
    store.seed_caller("dave", &Grants::admin()).await;
    let dave = Caller {
        credential_id: "dave",
        user_id: "dave",
    };
    invite_and_redeem(&store, dave, "frank").await;
    let delete = store.delete_user(Actor::Daemon, "dave", Cascade::Yes);
    delete.await.expect("the daemon deletes dave");
    assert!(!passkey("frank-key").await && !live(&store, "frank-session").await);
    let frank = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_users WHERE id='frank'"#)
        .fetch_optional(&store.pool)
        .await
        .expect("accounts are readable");
    assert!(frank.is_some(), "accounts are never deleted");
    assert!(passkey("erin-key").await && live(&store, "erin").await);
}

/// Gives the administrator `user` a passkey whose session invited
/// `<user>-guest`, then removes that passkey alone and logs the session
/// out, so nothing live of `user` leads to the guest any more.
async fn ended_issuer(store: &Store, user: &'static str) {
    store.admin_with_passkey(user).await;
    let alone = store.revoke_credential(Actor::Daemon, user, Cascade::No);
    alone.await.expect("the login is revoked alone");
    let (key, session) = (format!("{user}-key"), format!("{user}-signed-in"));
    let signed_in = proved(credential(&session, CredentialKind::Browser));
    let stored = |stored: &str| Ok::<_, StoreError>(stored.to_owned());
    let sign_in = store.sign_in_with_passkey(&key, user, stored, &signed_in);
    let sign_in = sign_in.await.expect("the passkey signs in");
    sign_in.expect("the passkey is its user's");
    let by_session = Caller {
        credential_id: &session,
        user_id: user,
    };
    invite_and_redeem(store, by_session, &format!("{user}-guest")).await;
    let remove = store.remove_passkey(Actor::Daemon, &key, Cascade::No);
    remove.await.expect("the passkey is removed alone");
    let logout = store.revoke_credential(Actor::Daemon, &session, Cascade::No);
    logout.await.expect("the session logs out");
}

/// Revoking all of an account's credentials, and deleting it, also start
/// from its sessions that ended, even after the passkey that signed them in
/// went and pruning ran, so what they issued goes too.
#[tokio::test]
async fn bulk_revocations_reach_what_ended_credentials_issued() {
    let (_dir, store) = Store::temporary().await;
    // An administrator nothing here touches, so no cascade locks out.
    store.admin_with_passkey("erin").await;
    ended_issuer(&store, "alice").await;
    ended_issuer(&store, "dave").await;
    store.prune_auth().await.expect("housekeeping runs");

    let all = store.revoke_credentials(Actor::Daemon, "alice", Cascade::Yes);
    all.await
        .expect("the daemon revokes all of alice's credentials");
    assert!(!live(&store, "alice-guest-session").await);
    let delete = store.delete_user(Actor::Daemon, "dave", Cascade::Yes);
    delete.await.expect("the daemon deletes dave");
    assert!(!live(&store, "dave-guest-session").await);
    assert_eq!(passkeys(&store).await, ["erin-key"]);
}
