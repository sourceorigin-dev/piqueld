//! Issuance lineage: which credential, invitation or passkey issued each
//! credential, invitation and passkey, kept in `auth_lineage`.
//!
//! Every issuing write records its node in the same transaction, naming the
//! credential that authorized it, the invitation it redeemed, or the passkey
//! it signed in with. Nodes outlive their rows while anything live descends
//! from them, so a lineage stays connected through sessions that logged out,
//! expired or were pruned, and through redeemed invitations. Authentication
//! never reads it.
use crate::{Store, StoreError};
use sqlx::SqliteConnection;

/// Whether revoking something also revokes what was issued through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cascade {
    /// Also revoke everything it issued, and everything they issued in turn,
    /// in any account, even through issuers that have since ended.
    Yes,
    /// Revoke only what was named, leaving what it issued valid.
    No,
}

impl From<bool> for Cascade {
    fn from(cascade: bool) -> Self {
        if cascade { Self::Yes } else { Self::No }
    }
}

/// A credential, invitation or passkey, as a node of the issuance tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issued<'a> {
    /// A browser session, CLI login or API token, by ID.
    Credential(&'a str),
    /// An account invitation or enrollment link, by ID.
    Invitation(&'a str),
    /// A registered passkey, by its `WebAuthn` credential ID.
    Passkey(&'a str),
}

impl<'a> Issued<'a> {
    /// The node's stored kind and ID.
    const fn key(self) -> (&'static str, &'a str) {
        match self {
            Self::Credential(id) => ("credential", id),
            Self::Invitation(id) => ("invitation", id),
            Self::Passkey(id) => ("passkey", id),
        }
    }

    /// Records this newly stored credential, invitation or passkey as issued
    /// by `issuer`, or as a root for `None`. Live rows the store wrote always
    /// have a node; an issuer written around it, e.g. by hand, gets one as a
    /// root, so the new edge is still recorded.
    pub(super) async fn record(
        self,
        db: &mut SqliteConnection,
        issuer: Option<Issued<'_>>,
    ) -> Result<(), StoreError> {
        let parent = match issuer {
            Some(issuer) => {
                let (kind, id) = issuer.key();
                sqlx::query!(
                    "INSERT OR IGNORE INTO auth_lineage(kind,id) VALUES(?1,?2)",
                    kind,
                    id
                )
                .execute(&mut *db)
                .await
                .map_err(StoreError::database)?;
                let node = sqlx::query_scalar!(
                    "SELECT node FROM auth_lineage WHERE kind=?1 AND id=?2",
                    kind,
                    id
                )
                .fetch_one(&mut *db)
                .await
                .map_err(StoreError::database)?;
                Some(node)
            }
            None => None,
        };
        let (kind, id) = self.key();
        sqlx::query!(
            "INSERT INTO auth_lineage(kind,id,issued_by) VALUES(?1,?2,?3)",
            kind,
            id,
            parent
        )
        .execute(db)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }
}

impl Store {
    /// Deletes `roots` and, with [`Cascade::Yes`], every credential,
    /// invitation and passkey issued through them. Callers have authorized
    /// the roots; their authority reaches all they issued. Refuses with
    /// [`super::Lockout::LastAdmin`] when removed passkeys leave no
    /// administrator able to sign in. Accounts are never deleted.
    pub(super) async fn revoke_on(
        db: &mut SqliteConnection,
        roots: &[Issued<'_>],
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let roots = serde_json::to_string(&roots.iter().map(|root| root.key()).collect::<Vec<_>>())
            .expect("pairs of strings serialize as JSON");
        // `[[kind, id], …]`: the roots, then what descends from them.
        let targets = match cascade {
            Cascade::No => roots,
            Cascade::Yes => sqlx::query_scalar!(
                r#"WITH RECURSIVE tree(kind,id) AS (
                    SELECT value->>0, value->>1 FROM json_each(?1)
                    UNION
                    SELECT child.kind, child.id FROM tree
                    JOIN auth_lineage parent ON parent.kind=tree.kind AND parent.id=tree.id
                    JOIN auth_lineage child ON child.issued_by=parent.node
                )
                SELECT json_group_array(json_array(kind,id)) AS "targets!: String" FROM tree"#,
                roots
            )
            .fetch_one(&mut *db)
            .await
            .map_err(StoreError::database)?,
        };
        sqlx::query!(
            "DELETE FROM auth_credentials WHERE id IN (SELECT value->>1 FROM json_each(?1) WHERE value->>0='credential')",
            targets
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM auth_invitations WHERE id IN (SELECT value->>1 FROM json_each(?1) WHERE value->>0='invitation')",
            targets
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?;
        let passkeys = sqlx::query!(
            "DELETE FROM auth_passkeys WHERE id IN (SELECT value->>1 FROM json_each(?1) WHERE value->>0='passkey')",
            targets
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if passkeys > 0 {
            Self::require_an_admin(db).await?;
        }
        Ok(())
    }

    /// Deletes the lineage nodes that neither name a live credential,
    /// invitation or passkey nor have a descendant that does. A removed
    /// node's whole subtree goes with it, so no remaining node loses its
    /// issuer.
    pub(super) async fn forget_lineage_on(db: &mut SqliteConnection) -> Result<(), StoreError> {
        sqlx::query!(
            "WITH RECURSIVE needed(node) AS (
                SELECT node FROM auth_lineage l WHERE
                    (kind='credential' AND EXISTS(SELECT 1 FROM auth_credentials WHERE id=l.id))
                    OR (kind='invitation' AND EXISTS(SELECT 1 FROM auth_invitations WHERE id=l.id))
                    OR (kind='passkey' AND EXISTS(SELECT 1 FROM auth_passkeys WHERE id=l.id))
                UNION
                SELECT l.issued_by FROM auth_lineage l JOIN needed n ON l.node=n.node
                WHERE l.issued_by IS NOT NULL
            )
            DELETE FROM auth_lineage WHERE node NOT IN (SELECT node FROM needed)"
        )
        .execute(db)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        let Actor::Account(alice) = store.seed_caller("alice", &Grants::admin()).await else {
            unreachable!("seeded an account caller")
        };
        let passkey = |id| NewPasskey {
            id,
            name: id,
            credential: "{}",
        };
        let own = PasskeyOwner::Existing(alice, "alice");
        assert!(store.add_passkey(own, passkey("alice-key")).await.unwrap());
        let invitation = NewInvitation {
            id: "invitation".into(),
            secret_hash: "invitation".into(),
            expires_at: now_secs() + 3600,
        };
        let none = Grants::default();
        store
            .create_invitation(alice, &invitation, &none)
            .await
            .unwrap();
        let bob = User {
            id: "bob".into(),
            username: "bob".into(),
            display_name: String::new(),
        };
        let redeem = PasskeyOwner::Redeem {
            user: &bob,
            invitation_hash: "invitation",
            session: credential("redeemed", CredentialKind::Browser),
        };
        assert!(store.add_passkey(redeem, passkey("bob-key")).await.unwrap());
        let mut token = credential("token", CredentialKind::Token);
        token.grants = Some(&none);
        let redeemed = Caller {
            credential_id: "redeemed",
            user_id: "bob",
        };
        store.create_token(redeemed, &token, None).await.unwrap();
        let logout = store.revoke_credential(Actor::Daemon, "redeemed", Cascade::No);
        logout.await.unwrap();
        let other = credential("other", CredentialKind::Cli);
        store.insert_credential("alice", &other).await.unwrap();
        let sibling = credential("sibling", CredentialKind::Cli);
        let other = Caller {
            credential_id: "other",
            user_id: "alice",
        };
        let issue = store.issue_for_credential_owner(other, &sibling);
        issue.await.unwrap().unwrap();
        let live = async |id| store.credential_owner(id).await.unwrap().is_some();
        let passkeys = async || {
            sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_passkeys ORDER BY id"#)
                .fetch_all(&store.pool)
                .await
                .unwrap()
        };

        // Alice's own passkey descends from her login, and nobody else could
        // administer the installation without it.
        let revoke = store.revoke_credential(Actor::Daemon, "alice", Cascade::Yes);
        assert!(matches!(
            revoke.await,
            Err(StoreError::Lockout(crate::Lockout::LastAdmin))
        ));
        assert!(live("alice").await && live("token").await);
        assert_eq!(passkeys().await, ["alice-key", "bob-key"]);

        let carol = store.seed_caller("carol", &Grants::admin()).await;
        let Actor::Account(carol) = carol else {
            unreachable!("seeded an account caller")
        };
        let own = PasskeyOwner::Existing(carol, "carol");
        assert!(store.add_passkey(own, passkey("carol-key")).await.unwrap());
        let revoke = store.revoke_credential(Actor::Daemon, "alice", Cascade::Yes);
        revoke.await.unwrap();
        assert!(!live("alice").await && !live("token").await);
        assert_eq!(passkeys().await, ["carol-key"]);
        assert!(live("other").await && live("sibling").await);
        let bob = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM auth_users WHERE id='bob'"#)
            .fetch_optional(&store.pool)
            .await
            .unwrap();
        assert!(bob.is_some(), "accounts are never deleted");

        let alone = store.revoke_credential(Actor::Daemon, "other", Cascade::No);
        alone.await.unwrap();
        assert!(!live("other").await && live("sibling").await);
    }
}
