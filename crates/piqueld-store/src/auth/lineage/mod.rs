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

/// What of an account's lineage a bulk revocation starts from. Nodes
/// remember their owner, so both reach what the account's ended
/// credentials issued, whether or not pruning removed their rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountRoots {
    /// Every credential the account ever had that something still descends
    /// from, or that is still stored: revoking all of them.
    Credentials,
    /// Its credentials and passkeys alike: deleting the account.
    Everything,
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

    /// `nodes` as the JSON `[[kind, id], …]` that `revoke_on` binds.
    #[expect(
        clippy::expect_used,
        reason = "A list of string pairs always serializes to JSON."
    )]
    fn json(nodes: &[Self]) -> String {
        let keys = nodes.iter().map(|node| node.key()).collect::<Vec<_>>();
        serde_json::to_string(&keys).expect("string pairs serialize as JSON")
    }

    /// Records this newly stored credential, invitation or passkey, owned by
    /// the account `owner` (`None` for invitations), as issued by `issuer`,
    /// or as a root for `None`. Live rows the store wrote always have a node;
    /// an issuer written around it, e.g. by hand, gets one as a root, so the
    /// new edge is still recorded.
    pub(super) async fn record(
        self,
        db: &mut SqliteConnection,
        owner: Option<&str>,
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
            "INSERT INTO auth_lineage(kind,id,issued_by,user_id) VALUES(?1,?2,?3,?4)",
            kind,
            id,
            parent,
            owner
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
        Self::revoke_keys_on(db, Issued::json(roots), cascade).await
    }

    /// Revokes like [`Self::revoke_on`], starting from `roots` of the account
    /// `user_id`: its nodes, ended or not, and its stored rows.
    pub(super) async fn revoke_account_on(
        db: &mut SqliteConnection,
        user_id: &str,
        roots: AccountRoots,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let credentials_only = roots == AccountRoots::Credentials;
        let roots = sqlx::query_scalar!(
            r#"SELECT json_group_array(json_array(kind,id)) AS "roots!: String" FROM (
                SELECT kind,id FROM auth_lineage
                WHERE user_id=?1 AND (kind='credential' OR NOT ?2)
                UNION SELECT 'credential',id FROM auth_credentials WHERE user_id=?1
                UNION SELECT 'passkey',id FROM auth_passkeys WHERE user_id=?1 AND NOT ?2
            )"#,
            user_id,
            credentials_only
        )
        .fetch_one(&mut *db)
        .await
        .map_err(StoreError::database)?;
        Self::revoke_keys_on(db, roots, cascade).await
    }

    /// Revokes `roots`, as JSON `[[kind, id], …]` (see [`Self::revoke_on`]).
    async fn revoke_keys_on(
        db: &mut SqliteConnection,
        roots: String,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
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
mod tests;
