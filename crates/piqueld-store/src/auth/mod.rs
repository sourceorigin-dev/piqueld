//! Accounts, passkeys, revocable credentials, and invitations.
//!
//! Timestamps are Unix seconds. Callers pass only hashes of credential and
//! invitation secrets, so this module never sees a usable secret. Writes queue
//! behind the shared writer, and account changes refuse to lock everyone out.
//! Changes to an account take the [`Caller`], whose authority over the account
//! is checked against both sides' current grants inside the same transaction.
pub(super) mod access;
mod lineage;
mod security;
pub use security::SecurityEvent;

use super::{
    Actor, Attribution, AuditPrincipal, AuditRequest, Author, Store, StoreError, now_secs,
};
use access::{Authority, Caller, Holder};
pub use lineage::Cascade;
use lineage::{AccountRoots, Issued};
use piqueld_core::access::{Denied, GlobalPermission, Grants, Permission, Reauthenticate};
use piqueld_core::auth::{
    Account, CredentialView, Directory, HostOperator, InvitationView, OperatorSessionView,
    PasskeyView, User,
};
use piqueld_core::tailnet::TailnetBinding;
use sqlx::SqliteConnection;
use std::fmt;

/// Browser sessions end after a day without use, even before they expire.
pub(super) const SESSION_IDLE_SECS: i64 = 86_400;

/// Parses a credential's stored tailnet binding.
fn binding(stored: Option<String>) -> Result<Option<TailnetBinding>, StoreError> {
    stored
        .map(TailnetBinding::try_from)
        .transpose()
        .map_err(|reason| {
            StoreError::CorruptSource(format!("auth_credentials.tailnet: {reason}").into())
        })
}

/// Account changes refused because nobody could administer the installation
/// afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lockout {
    /// No account would hold `admin` on every application together with a
    /// passkey to sign in with.
    LastAdmin,
}

impl Lockout {
    /// Human-readable reason, suitable for API responses.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::LastAdmin => {
                "at least one account must keep admin on every application and a passkey"
            }
        }
    }
}

impl fmt::Display for Lockout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Lockout {}

/// Credential classes. Only browser sessions have an idle timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// Dashboard session cookie.
    Browser,
    /// Session approved for the command-line client.
    Cli,
    /// Named API token.
    Token,
}

impl CredentialKind {
    /// Value stored in the `kind` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Cli => "cli",
            Self::Token => "token",
        }
    }

    /// Parses a stored kind, reporting unknown values as corruption.
    pub(super) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "browser" => Ok(Self::Browser),
            "cli" => Ok(Self::Cli),
            "token" => Ok(Self::Token),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// How recently a credential's holder proved itself with a passkey, which
/// sensitive actions, like creating credentials, require (see
/// [`Proof::require_fresh`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    /// Credential class; API tokens are never fresh.
    pub kind: CredentialKind,
    /// When a passkey sign-in or step-up last proved the holder present, in
    /// Unix seconds; `None` when none has.
    pub authenticated_at: Option<i64>,
}

impl Proof {
    /// Requires a passkey sign-in or step-up within the last
    /// [`Reauthenticate::WINDOW_SECS`] before `now`, returning when it was. A
    /// missing or future time fails closed, and API tokens always fail, as no
    /// passkey confirms them.
    ///
    /// # Errors
    /// Returns [`Denied::Reauthenticate`] with the reason.
    pub fn require_fresh(self, now: i64) -> Result<i64, Denied> {
        match (self.kind, self.authenticated_at) {
            (CredentialKind::Token, _) => Err(Denied::Reauthenticate(Reauthenticate::Token)),
            (_, Some(at)) if (0..=Reauthenticate::WINDOW_SECS).contains(&(now - at)) => Ok(at),
            _ => Err(Denied::Reauthenticate(Reauthenticate::Stale)),
        }
    }
}

/// A credential to store for an account. Only its secret's hash is persisted.
pub struct NewCredential<'a> {
    /// Public identifier, used to revoke the credential.
    pub id: String,
    /// Hash of the bearer secret; the secret itself is never stored.
    pub secret_hash: String,
    /// Credential class, deciding idle-timeout behavior.
    pub kind: CredentialKind,
    /// Human-readable label shown in the account directory.
    pub name: &'a str,
    /// Absolute expiry in Unix seconds; `None` never expires.
    pub expires_at: Option<i64>,
    /// Grants a scoped credential is limited to; `None` acts with the
    /// account's full access.
    pub grants: Option<&'a Grants>,
    /// When a passkey proved the holder present, in Unix seconds: for a
    /// session a passkey sign-in opens, the time of that sign-in; for a CLI
    /// login a device approval issues, the approver's, which the approval
    /// recorded. `None` otherwise.
    pub authenticated_at: Option<i64>,
}

/// When a credential or link expires, in Unix seconds, as audit records
/// name it: an RFC 3339 UTC time, or `never`.
struct Expiry(Option<i64>);

impl fmt::Display for Expiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(at) => {
                let at = piqueld_core::observability::UtcTime(at.saturating_mul(1000));
                f.write_str(&at.rfc3339())
            }
            None => f.write_str("never"),
        }
    }
}

impl NewCredential<'_> {
    /// The credential as the audit record of its creation names it, never
    /// its secret: its kind, ID, name, grants, expiry and the tailnet
    /// identity it is bound to, e.g. `token "0190…" "deploy" grants
    /// (apps:deploy) expires never tailnet none`.
    fn describe(&self, tailnet: Option<&str>) -> String {
        let grants = self.grants.map_or_else(
            || "of its account".to_owned(),
            |grants| format!("({grants})"),
        );
        let expires = Expiry(self.expires_at);
        let tailnet = tailnet.unwrap_or("none");
        let (kind, id, name) = (self.kind.as_str(), &self.id, self.name);
        format!("{kind} {id:?} {name:?} grants {grants} expires {expires} tailnet {tailnet}")
    }

    /// Inserts the credential for `user_id`, marking it used now, as issued
    /// by `issuer` (see [`Issued::record`]). A duplicate ID or hash maps to
    /// `AlreadyExists`, and a missing account to `NotFound`.
    async fn insert(
        &self,
        db: &mut SqliteConnection,
        user_id: &str,
        issuer: Option<Issued<'_>>,
    ) -> Result<(), StoreError> {
        let now = now_secs();
        let kind = self.kind.as_str();
        let scoped = self.grants.is_some();
        sqlx::query!(
            "INSERT INTO auth_credentials(id,user_id,secret_hash,kind,name,created_at,last_used_at,expires_at,scoped,authenticated_at) VALUES(?1,?2,?3,?4,?5,?6,?6,?7,?8,?9)",
            self.id,
            user_id,
            self.secret_hash,
            kind,
            self.name,
            now,
            self.expires_at,
            scoped,
            self.authenticated_at
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::constraint)?;
        if let Some(grants) = self.grants {
            Holder::Credential(&self.id).replace(db, grants).await?;
        }
        Issued::Credential(&self.id)
            .record(db, Some(user_id), issuer)
            .await
    }
}

/// A registered `WebAuthn` credential, serialized by the caller.
pub struct NewPasskey<'a> {
    /// `WebAuthn` credential ID.
    pub id: &'a str,
    /// Human-readable label.
    pub name: &'a str,
    /// Serialized passkey state, updated after each sign-in.
    pub credential: &'a str,
}

impl NewPasskey<'_> {
    /// Inserts the passkey for `user_id` as issued by `issuer` (see
    /// [`Issued::record`]). A duplicate ID maps to `AlreadyExists`.
    async fn insert(
        &self,
        db: &mut SqliteConnection,
        user_id: &str,
        issuer: Option<Issued<'_>>,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO auth_passkeys(id,user_id,name,credential,created_at) VALUES(?1,?2,?3,?4,?5)",
            self.id,
            user_id,
            self.name,
            self.credential,
            now
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::constraint)?;
        Issued::Passkey(self.id)
            .record(db, Some(user_id), issuer)
            .await
    }
}

/// Who a newly registered passkey belongs to.
pub enum PasskeyOwner<'a> {
    /// A signed-in caller adds another passkey to its own account, named by
    /// ID; refused unless the caller's credential still belongs to it, is
    /// unscoped, and recently signed in with a passkey.
    Existing(Caller<'a>, &'a str),
    /// Registration redeems the setup secret or an invitation, and signs the
    /// account in with `session`. Account invitations create `user` with the
    /// invitation's grants; the setup secret creates it as administrator.
    /// Enrollment invitations add the passkey to their existing account
    /// instead, ignoring `user`.
    Redeem {
        /// Account to create, for setup and account invitations.
        user: &'a User,
        /// Hash of the setup secret or invitation secret being redeemed.
        invitation_hash: &'a str,
        /// Browser session opened for the account.
        session: NewCredential<'a>,
    },
}

/// What a redemption consumed.
enum Redeemed {
    /// The setup secret or the admin recovery link, which no credential
    /// issued, creating an administrator: `how`, e.g. "first-account setup".
    Root(&'static str),
    /// An account invitation, by ID, creating an account with `grants`.
    Invitation {
        /// The invitation's ID.
        id: String,
        /// The grants the created account received.
        grants: Grants,
    },
    /// An enrollment link, by ID, adding a passkey to its account.
    Enrollment(String),
}

impl Redeemed {
    /// The lineage node that issued what the redemption creates.
    fn issuer(&self) -> Option<Issued<'_>> {
        match self {
            Self::Root(_) => None,
            Self::Invitation { id, .. } | Self::Enrollment(id) => Some(Issued::Invitation(id)),
        }
    }

    /// What redeeming it for the account `id`, now named `username`, did,
    /// as its audit record names it, never the secret it consumed.
    fn describe(&self, id: &str, username: &str) -> String {
        match self {
            Self::Root(how) => {
                let admin = Grants::admin();
                format!("account {id:?} {username:?} created by {how} with grants ({admin})")
            }
            Self::Invitation {
                id: invitation,
                grants,
            } => format!(
                "account {id:?} {username:?} created by invitation {invitation:?} with grants ({grants})"
            ),
            Self::Enrollment(enrollment) => {
                format!("account {id:?} {username:?} enrolled by {enrollment:?}")
            }
        }
    }
}

/// What an unused setup secret or invitation will do once redeemed.
pub enum Invitation {
    /// Creates a new account: the first administrator for the setup secret,
    /// otherwise with the invitation's grants.
    Account,
    /// Adds a passkey to this existing account.
    Enrollment(User),
}

/// An invitation to store, by the hash of its secret. The caller creating it
/// becomes its issuer.
pub struct NewInvitation {
    /// Revocation identifier.
    pub id: String,
    /// Hash of the secret in the shared link.
    pub secret_hash: String,
    /// Absolute expiry in Unix seconds.
    pub expires_at: i64,
}

/// The part of the account directory a viewer may see.
#[derive(Clone, Copy)]
pub enum DirectoryView<'a> {
    /// Every account, invitation, and host operator session.
    All,
    /// One account, by ID, with its passkeys and credentials.
    Account(&'a str),
    /// The caller's account, without passkeys, and only the caller's own
    /// credential: what a credential limited to its own grants sees.
    Credential(Caller<'a>),
}

impl<'a> DirectoryView<'a> {
    /// The account and the credential the view is limited to, if any.
    const fn limits(self) -> (Option<&'a str>, Option<&'a str>) {
        match self {
            Self::All => (None, None),
            Self::Account(user) => (Some(user), None),
            Self::Credential(caller) => (Some(caller.user_id), Some(caller.credential_id)),
        }
    }
}

/// The account behind a live credential.
pub struct CredentialOwner {
    /// Credential class and its last passkey proof.
    pub proof: Proof,
    /// Matched credential, used to record its use or revoke it.
    pub credential_id: String,
    /// Account that owns the credential.
    pub user: User,
    /// The credential's effective grants.
    pub grants: Grants,
    /// Whether the credential is limited to its own grants.
    pub scoped: bool,
    /// Last recorded use in Unix seconds, letting callers skip frequent refreshes.
    pub last_used_at: i64,
    /// Tailnet user or tag the credential is bound to, if any.
    pub tailnet: Option<TailnetBinding>,
}

impl Store {
    /// Whether first-account setup has completed.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn auth_initialized(&self) -> Result<bool, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT initialized AS "initialized: bool" FROM auth_setup WHERE singleton=1"#
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Replaces the setup secret while the installation is unclaimed.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn set_setup_secret(&self, hash: &str) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "UPDATE auth_setup SET secret_hash=?1 WHERE singleton=1 AND initialized=0",
            hash
        ))
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }

    /// Replaces the admin recovery link with one for `hash`, valid until
    /// `expires_at`, and records that `operator` asked for it as a security
    /// event.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn create_recovery(
        &self,
        request: Option<&AuditRequest>,
        hash: &str,
        expires_at: i64,
        operator: HostOperator,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO auth_recovery(singleton,secret_hash,expires_at) VALUES(1,?1,?2)
            ON CONFLICT(singleton) DO UPDATE SET secret_hash=excluded.secret_hash,expires_at=excluded.expires_at",
            hash,
            expires_at
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let issued = SecurityEvent::RecoveryIssued;
        Self::operator_link_event_on(&mut tx, issued, "An admin recovery link", operator).await?;
        let change = format!(
            "admin recovery link issued, expires {}",
            Expiry(Some(expires_at))
        );
        Self::operator_author(operator, request)
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// The host operator over the Unix socket, acting through `request`.
    const fn operator_author(operator: HostOperator, request: Option<&AuditRequest>) -> Author<'_> {
        Author {
            actor: Actor::Operator {
                operator,
                session: None,
            },
            request,
        }
    }

    /// Records that `operator` was issued a link, e.g. `An admin recovery
    /// link`, as a security event attributed to it.
    async fn operator_link_event_on(
        db: &mut SqliteConnection,
        event: SecurityEvent,
        link: &str,
        operator: HostOperator,
    ) -> Result<(), StoreError> {
        let message = format!(
            "{link} was issued over the Unix socket to uid {}",
            operator.uid
        );
        let actor = Actor::Operator {
            operator,
            session: None,
        };
        Self::security_event_on(db, event, &message, actor.attribution()).await
    }

    /// Stores a host operator sign-in link for `hash`, valid until
    /// `expires_at`, and records that `operator` asked for it as a security
    /// event. [`Self::prune_auth`] removes it once expired.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn create_operator_link(
        &self,
        request: Option<&AuditRequest>,
        id: &str,
        hash: &str,
        expires_at: i64,
        operator: HostOperator,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO auth_operator_sessions(id,secret_hash,uid,expires_at) VALUES(?1,?2,?3,?4)",
            id,
            hash,
            operator.uid,
            expires_at
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::constraint)?;
        let issued = SecurityEvent::OperatorSignInIssued;
        Self::operator_link_event_on(&mut tx, issued, "A host operator sign-in link", operator)
            .await?;
        let expires = Expiry(Some(expires_at));
        let change = format!("host operator sign-in link {id:?} issued, expires {expires}");
        Self::operator_author(operator, request)
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Redeems the live sign-in link `link_hash` as a host operator browser
    /// session with secret hash `session_hash`, valid until `expires_at`,
    /// recording the sign-in as made through `request`.
    /// Returns the session's ID and operator, or `None` when the link was
    /// already used, has expired, or never existed. Such links are turned
    /// away by a read, so guessing them never waits for the writer.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn redeem_operator_link(
        &self,
        request: Option<&AuditRequest>,
        link_hash: &str,
        session_hash: &str,
        expires_at: i64,
    ) -> Result<Option<(String, HostOperator)>, StoreError> {
        let now = now_secs();
        let live = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_operator_sessions WHERE secret_hash=?1 AND redeemed=0 AND expires_at>?2) AS "live!: bool""#,
            link_hash,
            now
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        if !live {
            return Ok(None);
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let row = sqlx::query!(
            r#"UPDATE auth_operator_sessions SET secret_hash=?2,redeemed=1,expires_at=?3
            WHERE secret_hash=?1 AND redeemed=0 AND expires_at>?4 RETURNING id AS "id!",uid"#,
            link_hash,
            session_hash,
            expires_at,
            now
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::constraint)?;
        let Some(row) = row else {
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(None);
        };
        let operator = Self::host_operator(row.uid)?;
        let change = format!("host operator session {:?} signed in", row.id);
        let author = Author { actor: (), request };
        let who = AuditPrincipal::Operator(operator);
        author.commit_signed_in(tx, who, &change).await?;
        Ok(Some((row.id, operator)))
    }

    /// The live host operator session with secret hash `hash`: its ID and
    /// operator.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn operator_session(
        &self,
        hash: &str,
    ) -> Result<Option<(String, HostOperator)>, StoreError> {
        let now = now_secs();
        let row = sqlx::query!(
            r#"SELECT id AS "id!",uid FROM auth_operator_sessions WHERE secret_hash=?1 AND redeemed=1 AND expires_at>?2"#,
            hash,
            now
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        row.map(|row| Ok((row.id, Self::host_operator(row.uid)?)))
            .transpose()
    }

    /// Whether a host operator link or session with this ID exists.
    async fn operator_session_on(db: &mut SqliteConnection, id: &str) -> Result<bool, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_operator_sessions WHERE id=?1) AS "exists!: bool""#,
            id
        )
        .fetch_one(db)
        .await
        .map_err(StoreError::database)
    }

    /// The host operator with a stored Unix user ID.
    pub(super) fn host_operator(uid: i64) -> Result<HostOperator, StoreError> {
        Ok(HostOperator {
            uid: u32::try_from(uid).map_err(StoreError::corrupt)?,
        })
    }

    /// Describes the open setup secret, admin recovery link, or live
    /// invitation matching a hash, or `None` when there is none.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn invitation(&self, hash: &str) -> Result<Option<Invitation>, StoreError> {
        let now = now_secs();
        let setup = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_setup WHERE initialized=0 AND secret_hash=?1)
            OR EXISTS(SELECT 1 FROM auth_recovery WHERE secret_hash=?1 AND expires_at>?2) AS "open!: bool""#,
            hash,
            now
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        if setup {
            return Ok(Some(Invitation::Account));
        }
        let Some(row) = sqlx::query!(
            r#"SELECT i.user_id,u.username AS "username?",u.display_name AS "display_name?" FROM auth_invitations i LEFT JOIN auth_users u ON u.id=i.user_id WHERE i.secret_hash=?1 AND i.expires_at>?2"#,
            hash,
            now
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        Ok(Some(match (row.user_id, row.username, row.display_name) {
            (Some(id), Some(username), Some(display_name)) => Invitation::Enrollment(User {
                id,
                username,
                display_name,
            }),
            (None, ..) => Invitation::Account,
            _ => return Err(StoreError::Corrupt),
        }))
    }

    /// Whether an account already uses `username`, regardless of case.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn username_taken(&self, username: &str) -> Result<bool, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_users WHERE username=?1) AS "taken!: bool""#,
            username
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Serialized `WebAuthn` credentials registered to one account.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn passkey_credentials(&self, user_id: &str) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar!(
            "SELECT credential FROM auth_passkeys WHERE user_id=?1",
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Stores a registered passkey. Redemption atomically consumes the setup
    /// secret or invitation, creates or selects the account, and opens its
    /// session. Returns `false` when the secret was already used or has
    /// expired, when an enrollment invitation targets another account, or when
    /// a signed-in caller adds a passkey to an account that is not its own.
    /// The passkey is issued by the caller's credential or the redeemed
    /// invitation, and the session by the passkey; both record themselves as
    /// made through `request`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn add_passkey(
        &self,
        request: Option<&AuditRequest>,
        owner: PasskeyOwner<'_>,
        passkey: NewPasskey<'_>,
    ) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let added = format!("passkey {:?} {:?}", passkey.id, passkey.name);
        match &owner {
            PasskeyOwner::Existing(caller, user_id) => {
                let authority = caller.load(&mut tx).await?;
                authority.require_unscoped()?;
                authority.require_fresh()?;
                if authority.user_id != *user_id {
                    return Ok(false);
                }
                let issuer = Issued::Credential(caller.credential_id);
                passkey.insert(&mut tx, user_id, Some(issuer), now).await?;
                let change = format!("{added} added to account {user_id:?}");
                let author = Author {
                    actor: *caller,
                    request,
                };
                let target = (None, None);
                author
                    .commit_recorded(tx, 200, Some(&change), target)
                    .await?;
            }
            PasskeyOwner::Redeem {
                user,
                invitation_hash,
                session,
            } => {
                let Some(redeemed) = Self::redeem_on(&mut tx, user, invitation_hash, now).await?
                else {
                    return Ok(false);
                };
                passkey
                    .insert(&mut tx, &user.id, redeemed.issuer(), now)
                    .await?;
                let issuer = Issued::Passkey(passkey.id);
                session.insert(&mut tx, &user.id, Some(issuer)).await?;
                // An enrolled account keeps its stored name, whatever the
                // ceremony started with.
                let username = match redeemed {
                    Redeemed::Enrollment(_) => Self::username_on(&mut tx, &user.id).await?,
                    Redeemed::Root(_) | Redeemed::Invitation { .. } => user.username.clone(),
                };
                let account = redeemed.describe(&user.id, &username);
                let change = format!("{account} with {added}; {}", session.describe(None));
                let who = AuditPrincipal::Account {
                    id: &user.id,
                    username: &username,
                };
                let author = Author { actor: (), request };
                author.commit_signed_in(tx, who, &change).await?;
            }
        }
        Ok(true)
    }

    /// Consumes the setup secret, the admin recovery link, or a live
    /// invitation for `user`:
    ///
    /// 1. The setup secret closes initial setup and creates `user` with `admin`
    ///    on every application; so does the admin recovery link.
    /// 2. An account invitation creates `user` with the invitation's grants.
    /// 3. An enrollment invitation must target `user`, which already exists.
    ///
    /// Invitations carry their issuer's authority, so they are only redeemed
    /// while the issuer could still create them: grants may have changed on
    /// either side since. Returns what was consumed, or `None` when nothing
    /// matching is left to redeem.
    async fn redeem_on(
        db: &mut SqliteConnection,
        user: &User,
        invitation_hash: &str,
        now: i64,
    ) -> Result<Option<Redeemed>, StoreError> {
        let setup = sqlx::query!(
            "UPDATE auth_setup SET initialized=1,secret_hash=NULL WHERE initialized=0 AND secret_hash=?1",
            invitation_hash
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        let recovery = sqlx::query!(
            "DELETE FROM auth_recovery WHERE secret_hash=?1 AND expires_at>?2",
            invitation_hash,
            now
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if setup == 1 || recovery == 1 {
            Self::insert_user_on(db, user, now).await?;
            let daemon = Attribution::default();
            let how = if setup == 1 {
                "first-account setup"
            } else {
                "an admin recovery link"
            };
            let by = format!("by {how}");
            Self::replace_user_grants_on(db, &user.id, &Grants::admin(), daemon, &by).await?;
            return Ok(Some(Redeemed::Root(how)));
        }
        let Some(invitation) = sqlx::query!(
            r#"SELECT id AS "id!",issuer_id,user_id FROM auth_invitations WHERE secret_hash=?1 AND expires_at>?2"#,
            invitation_hash,
            now
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        let authority = Holder::User(&invitation.issuer_id).grants(&mut *db).await?;
        let redeemed = if let Some(target) = invitation.user_id {
            let grants = Holder::User(&target).grants(&mut *db).await?;
            let own = invitation.issuer_id == target;
            if target != user.id || authority.may_change_account(own, &grants).is_err() {
                return Ok(None);
            }
            Redeemed::Enrollment(invitation.id)
        } else {
            let grants = Holder::Invitation(&invitation.id).grants(&mut *db).await?;
            if Self::may_invite(&authority, &grants).is_err() {
                return Ok(None);
            }
            Self::insert_user_on(db, user, now).await?;
            let issuer = Attribution {
                user_id: Some(&invitation.issuer_id),
                ..Attribution::default()
            };
            let how = "by invitation";
            Self::replace_user_grants_on(db, &user.id, &grants, issuer, how).await?;
            Redeemed::Invitation {
                id: invitation.id,
                grants,
            }
        };
        if let Some(Issued::Invitation(id)) = redeemed.issuer() {
            sqlx::query!("DELETE FROM auth_invitations WHERE id=?1", id)
                .execute(&mut *db)
                .await
                .map_err(StoreError::database)?;
        }
        Ok(Some(redeemed))
    }

    /// Creates an account; a taken username maps to `AlreadyExists`.
    async fn insert_user_on(
        db: &mut SqliteConnection,
        user: &User,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO auth_users(id,username,display_name,created_at) VALUES(?1,?2,?3,?4)",
            user.id,
            user.username,
            user.display_name,
            now
        )
        .execute(db)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }

    /// Verifies a passkey assertion and opens a browser session while holding
    /// the writer lock, so a concurrent deletion or replay cannot resurrect
    /// access. `verify` receives the stored credential and returns its updated
    /// form. The passkey issues the session, which is recorded as made
    /// through `request`, naming who signed in. Returns `None` when the
    /// passkey does not belong to `user_id`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn sign_in_with_passkey<E: From<StoreError>>(
        &self,
        request: Option<&AuditRequest>,
        passkey_id: &str,
        user_id: &str,
        verify: impl FnOnce(&str) -> Result<String, E>,
        session: &NewCredential<'_>,
    ) -> Result<Option<User>, E> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let Some(user) = Self::verify_passkey_on(&mut tx, passkey_id, user_id, verify).await?
        else {
            return Ok(None);
        };
        let issuer = Issued::Passkey(passkey_id);
        session.insert(&mut tx, user_id, Some(issuer)).await?;
        let change = format!("{} with passkey {passkey_id:?}", session.describe(None));
        let who = AuditPrincipal::Account {
            id: &user.id,
            username: &user.username,
        };
        let author = Author { actor: (), request };
        author.commit_signed_in(tx, who, &change).await?;
        Ok(Some(user))
    }

    /// Verifies a passkey assertion for the caller's own account and marks
    /// the caller's credential as proved at `authenticated_at`, while holding
    /// the writer lock, as [`Self::sign_in_with_passkey`] does. Nothing else
    /// changes: not the credential's expiry or grants, nor other sessions.
    /// Returns `false` when the passkey is not the account's. API tokens are
    /// refused, as no passkey confirms them.
    ///
    /// # Errors
    /// Returns a revoked credential, a refusal, or storage errors.
    pub async fn confirm_with_passkey<E: From<StoreError>>(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        passkey_id: &str,
        verify: impl FnOnce(&str) -> Result<String, E>,
        authenticated_at: i64,
    ) -> Result<bool, E> {
        let author = author.into();
        let caller = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let authority = caller.load(&mut tx).await?;
        if authority.proof.kind == CredentialKind::Token {
            return Err(StoreError::Denied(Denied::Reauthenticate(Reauthenticate::Token)).into());
        }
        let user_id = authority.user_id.as_str();
        if Self::verify_passkey_on(&mut tx, passkey_id, user_id, verify)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        sqlx::query!(
            "UPDATE auth_credentials SET authenticated_at=?1 WHERE id=?2",
            authenticated_at,
            caller.credential_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let credential = caller.credential_id;
        let change = format!("credential {credential:?} confirmed with passkey {passkey_id:?}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        Ok(true)
    }

    /// Verifies an assertion by `user_id`'s passkey `passkey_id` and stores
    /// the passkey's updated state, e.g. its signature counter. `verify`
    /// receives the stored credential and returns its updated form. Returns
    /// the account, or `None` when the passkey does not belong to it.
    async fn verify_passkey_on<E: From<StoreError>>(
        db: &mut SqliteConnection,
        passkey_id: &str,
        user_id: &str,
        verify: impl FnOnce(&str) -> Result<String, E>,
    ) -> Result<Option<User>, E> {
        let Some(row) = sqlx::query!(
            r#"SELECT p.credential,u.id AS "id!",u.username,u.display_name FROM auth_passkeys p JOIN auth_users u ON u.id=p.user_id WHERE p.id=?1 AND u.id=?2"#,
            passkey_id,
            user_id
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        let credential = verify(&row.credential)?;
        sqlx::query!(
            "UPDATE auth_passkeys SET credential=?1 WHERE id=?2",
            credential,
            passkey_id
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?;
        Ok(Some(User {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
        }))
    }

    /// Finds the account for a live credential by its secret hash, with its
    /// effective grants read from the same snapshot. This is read-only, so
    /// authenticating never waits for writers.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn credential_owner(
        &self,
        hash: &str,
    ) -> Result<Option<CredentialOwner>, StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let mut tx = self.snapshot().await?;
        let row = sqlx::query!(
            r#"SELECT c.id AS "credential_id!",c.kind,c.authenticated_at,c.last_used_at,c.scoped AS "scoped: bool",c.tailnet,u.id AS "id!",u.username,u.display_name FROM auth_credentials c JOIN auth_users u ON u.id=c.user_id WHERE c.secret_hash=?1 AND (c.expires_at IS NULL OR c.expires_at>?2) AND (c.kind!='browser' OR c.last_used_at>?3)"#,
            hash,
            now,
            idle
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let proof = Proof {
            kind: CredentialKind::parse(&row.kind)?,
            authenticated_at: row.authenticated_at,
        };
        let authority = Authority::read(
            &mut tx,
            row.id.clone(),
            &row.credential_id,
            row.scoped,
            proof,
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some(CredentialOwner {
            proof,
            credential_id: row.credential_id,
            user: User {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
            },
            grants: authority.grants,
            scoped: authority.scoped,
            last_used_at: row.last_used_at,
            tailnet: binding(row.tailnet)?,
        }))
    }

    /// Records use of a live credential. It queues behind other writers and
    /// judges liveness once it holds the writer lock, so a revocation or expiry
    /// that lands while it waits wins. Returns `false` when the credential was
    /// revoked or expired in the meantime.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn touch_credential(&self, id: &str) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let rows = sqlx::query!(
            "UPDATE auth_credentials SET last_used_at=MAX(last_used_at,?1) WHERE id=?2 AND (expires_at IS NULL OR expires_at>?1) AND (kind!='browser' OR last_used_at>?3)",
            now,
            id,
            idle
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(rows == 1)
    }

    /// Issues `credential` to the account behind the approving credential, if
    /// the approver may still create credentials and holds every grant the new
    /// one is limited to, and the passkey proof the approval recorded, the
    /// credential's `authenticated_at`, is still recent. The approver issues
    /// it, recorded as signing its account in through the author's request.
    /// Returns `None` when the approving session has ended.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn issue_for_credential_owner(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        credential: &NewCredential<'_>,
    ) -> Result<Option<User>, StoreError> {
        let author = author.into();
        let approver = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let authority = match approver.load(&mut tx).await {
            Ok(authority) => authority,
            Err(StoreError::CredentialRevoked) => return Ok(None),
            Err(error) => return Err(error),
        };
        authority.may_hand_out(credential.grants)?;
        // The approval's proof, which the new login inherits: the approver
        // confirming again later, for something else, doesn't extend it.
        let approval = Proof {
            authenticated_at: credential.authenticated_at,
            ..authority.proof
        };
        approval
            .require_fresh(now_secs())
            .map_err(StoreError::Denied)?;
        let user = sqlx::query_as!(
            User,
            r#"SELECT id AS "id!",username,display_name FROM auth_users WHERE id=?1"#,
            authority.user_id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let issuer = Issued::Credential(approver.credential_id);
        credential.insert(&mut tx, &user.id, Some(issuer)).await?;
        let approving = approver.credential_id;
        let change = format!("{} approved by {approving:?}", credential.describe(None));
        let who = AuditPrincipal::Account {
            id: &user.id,
            username: &user.username,
        };
        author.commit_signed_in(tx, who, &change).await?;
        Ok(Some(user))
    }

    /// Marks the approving account's own CLI login `credential_id` as proved
    /// at `authenticated_at`, the approval's passkey proof, if that proof is
    /// still recent and the approver's credential still live. A later proof
    /// the login already holds stays, so confirmations finishing out of
    /// order never make it stale early, and such a confirmation changes
    /// nothing and records nothing. Its expiry and grants stay as they are.
    /// Returns `false` when the login ended, so it is never revived.
    ///
    /// # Errors
    /// Returns a revoked approver, a refusal, or storage errors.
    pub async fn confirm_login(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        credential_id: &str,
        authenticated_at: i64,
    ) -> Result<bool, StoreError> {
        let author = author.into();
        let approver = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let authority = approver.load(&mut tx).await?;
        let approval = Proof {
            authenticated_at: Some(authenticated_at),
            ..authority.proof
        };
        let now = now_secs();
        approval.require_fresh(now).map_err(StoreError::Denied)?;
        let confirmed = sqlx::query!(
            "UPDATE auth_credentials SET authenticated_at=?1 WHERE id=?2 AND user_id=?3 AND kind='cli' AND (expires_at IS NULL OR expires_at>?4) AND (authenticated_at IS NULL OR authenticated_at<?1)",
            authenticated_at,
            credential_id,
            authority.user_id,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if confirmed == 0 {
            // Already proved as recently: live, but nothing to change.
            return sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM auth_credentials WHERE id=?1 AND user_id=?2 AND kind='cli' AND (expires_at IS NULL OR expires_at>?3)) AS "live!: bool""#,
                credential_id,
                authority.user_id,
                now
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database);
        }
        let username = Self::username_on(&mut tx, &authority.user_id).await?;
        let approving = approver.credential_id;
        let change = format!("cli {credential_id:?} confirmed, approved by {approving:?}");
        let who = AuditPrincipal::Account {
            id: &authority.user_id,
            username: &username,
        };
        author.commit_signed_in(tx, who, &change).await?;
        Ok(true)
    }

    /// Deletes one credential if `actor` may change its owner, with what it
    /// issued as `cascade` says, or one host operator session, which issues
    /// nothing, if `actor` could manage an administrator, as that session
    /// acts with `admin`. A credential may always revoke itself, even a
    /// scoped one. Unknown IDs are `NotFound`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn revoke_credential(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let owner = sqlx::query_scalar!("SELECT user_id FROM auth_credentials WHERE id=?1", id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let change = if let Some(owner) = owner {
            let own = matches!(actor, Actor::Account(caller) if caller.credential_id == id);
            if !own {
                Self::check_account_on(&mut tx, actor, &owner).await?;
            }
            let revoked = Self::revoke_on(&mut tx, &[Issued::Credential(id)], cascade).await?;
            format!("credential {id:?} of account {owner:?} revoked {cascade}: {revoked}")
        } else if Self::operator_session_on(&mut tx, id).await? {
            if let Some(caller) = actor.load(&mut tx).await? {
                caller
                    .grants
                    .may_change_account(false, &Grants::admin())
                    .map_err(StoreError::Denied)?;
            }
            sqlx::query!("DELETE FROM auth_operator_sessions WHERE id=?1", id)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            format!("host operator session {id:?} ended")
        } else {
            return Err(StoreError::NotFound);
        };
        self.commit_access_change(tx, author, Some(&change)).await
    }

    /// Deletes every stored credential of an account, signing it out
    /// everywhere, with what they and its ended credentials issued as
    /// `cascade` says (see [`AccountRoots::Credentials`]).
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn revoke_credentials(
        &self,
        author: impl Into<Author<'_>>,
        user_id: &str,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, author.actor, user_id).await?;
        let roots = AccountRoots::Credentials;
        let revoked = Self::revoke_account_on(&mut tx, user_id, roots, cascade).await?;
        let change = revoked
            .any()
            .then(|| format!("credentials of account {user_id:?} revoked {cascade}: {revoked}"));
        self.commit_access_change(tx, author, change.as_deref())
            .await
    }

    /// The part of the account directory `view` covers, read in one
    /// snapshot: accounts with their grants, passkeys and live credentials,
    /// and, for [`DirectoryView::All`], live invitations and host operator
    /// sessions. Secrets and their hashes are never included.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn auth_directory(&self, view: DirectoryView<'_>) -> Result<Directory, StoreError> {
        let now = now_secs();
        let (user, credential) = view.limits();
        let mut tx = self.snapshot().await?;
        let users = sqlx::query_as!(
            User,
            r#"SELECT id AS "id!",username,display_name FROM auth_users WHERE ?1 IS NULL OR id=?1 ORDER BY username"#,
            user
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut accounts = Vec::with_capacity(users.len());
        for user in users {
            let grants = Holder::User(&user.id).grants(&mut tx).await?;
            accounts.push(Account { user, grants });
        }
        let passkeys = sqlx::query_as!(
            PasskeyView,
            r#"SELECT id AS "id!",user_id,name FROM auth_passkeys WHERE (?1 IS NULL OR user_id=?1) AND ?2 IS NULL ORDER BY created_at"#,
            user,
            credential
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",user_id,kind,name,last_used_at,expires_at,scoped AS "scoped: bool",tailnet FROM auth_credentials WHERE (expires_at IS NULL OR expires_at>?1) AND (kind!='browser' OR last_used_at>?1-?2) AND (?3 IS NULL OR user_id=?3) AND (?4 IS NULL OR id=?4) ORDER BY created_at"#,
            now,
            SESSION_IDLE_SECS,
            user,
            credential
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut credentials = Vec::with_capacity(rows.len());
        for row in rows {
            let grants = if row.scoped {
                Some(Holder::Credential(&row.id).grants(&mut tx).await?)
            } else {
                None
            };
            credentials.push(CredentialView {
                id: row.id,
                user_id: row.user_id,
                kind: row.kind,
                name: row.name,
                last_used_at: row.last_used_at,
                expires_at: row.expires_at,
                grants,
                tailnet: binding(row.tailnet)?,
            });
        }
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",issuer_id,user_id,expires_at FROM auth_invitations WHERE expires_at>?1 AND ?2 IS NULL ORDER BY expires_at"#,
            now,
            user
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut invitations = Vec::with_capacity(rows.len());
        for row in rows {
            let grants = Holder::Invitation(&row.id).grants(&mut tx).await?;
            invitations.push(InvitationView {
                id: row.id,
                issuer_id: row.issuer_id,
                user_id: row.user_id,
                expires_at: row.expires_at,
                grants,
            });
        }
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",uid,expires_at FROM auth_operator_sessions WHERE redeemed=1 AND expires_at>?1 AND ?2 IS NULL ORDER BY expires_at"#,
            now,
            user
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let operator_sessions = rows
            .into_iter()
            .map(|row| {
                Ok(OperatorSessionView {
                    id: row.id,
                    operator: Self::host_operator(row.uid)?,
                    expires_at: row.expires_at,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Directory {
            users: accounts,
            passkeys,
            credentials,
            invitations,
            operator_sessions,
        })
    }

    /// Changes an account's username and display name. A taken username maps to
    /// `AlreadyExists`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn update_user(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
        username: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, author.actor, id).await?;
        let current = sqlx::query!(
            "SELECT username,display_name FROM auth_users WHERE id=?1",
            id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let changed = current.username != username || current.display_name != display_name;
        sqlx::query!(
            "UPDATE auth_users SET username=?1,display_name=?2 WHERE id=?3",
            username,
            display_name,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::constraint)?;
        let change = changed
            .then(|| format!("account {id:?} username {username:?} display name {display_name:?}"));
        author
            .commit_recorded(tx, 200, change.as_deref(), (None, None))
            .await
    }

    /// Deletes an account with its passkeys, credentials, grants, and
    /// invitations, and what its credentials and passkeys issued as
    /// `cascade` says. Refuses to leave no administrator able to sign in.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn delete_user(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, author.actor, id).await?;
        let username = Self::username_on(&mut tx, id).await?;
        let roots = AccountRoots::Everything;
        let revoked = Self::revoke_account_on(&mut tx, id, roots, cascade).await?;
        sqlx::query!("DELETE FROM auth_users WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        Self::require_an_admin(&mut tx).await?;
        let change = format!("account {id:?} {username:?} deleted {cascade}: {revoked}");
        self.commit_access_change(tx, author, Some(&change)).await
    }

    /// Checks that `actor` may change the account owning a passkey, and
    /// returns that account's ID. Unknown passkeys are `NotFound`.
    async fn check_passkey_owner_on(
        db: &mut SqliteConnection,
        actor: Actor<'_>,
        id: &str,
    ) -> Result<String, StoreError> {
        let owner = sqlx::query_scalar!("SELECT user_id FROM auth_passkeys WHERE id=?1", id)
            .fetch_optional(&mut *db)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
        Self::check_account_on(db, actor, &owner).await?;
        Ok(owner)
    }

    /// Deletes one passkey, with the sessions it signed in and what they
    /// issued as `cascade` says, refusing to leave no administrator able to
    /// sign in.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn remove_passkey(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
        cascade: Cascade,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let owner = Self::check_passkey_owner_on(&mut tx, author.actor, id).await?;
        let revoked = Self::revoke_on(&mut tx, &[Issued::Passkey(id)], cascade).await?;
        let change = format!("passkey {id:?} of account {owner:?} removed {cascade}: {revoked}");
        self.commit_access_change(tx, author, Some(&change)).await
    }

    /// Changes a passkey's label.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn rename_passkey(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
        name: &str,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let owner = Self::check_passkey_owner_on(&mut tx, author.actor, id).await?;
        let renamed = sqlx::query!(
            "UPDATE auth_passkeys SET name=?1 WHERE id=?2 AND name IS NOT ?1",
            name,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected()
            > 0;
        let change = renamed.then(|| format!("passkey {id:?} of account {owner:?} named {name:?}"));
        author
            .commit_recorded(tx, 200, change.as_deref(), (None, None))
            .await
    }

    /// Stores an account invitation that gives the created account `grants`,
    /// if `caller` may manage accounts, holds every grant, is not scoped, and
    /// recently signed in with a passkey: redeeming it yields a passkey and
    /// session. `add_passkey` consumes it.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn create_invitation(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        invitation: &NewInvitation,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let caller = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let authority = caller.load(&mut tx).await?;
        authority.require_unscoped()?;
        Self::may_invite(&authority.grants, grants)?;
        authority.require_fresh()?;
        let issuer = &authority.user_id;
        Self::insert_invitation_on(&mut tx, invitation, issuer, caller.credential_id, None).await?;
        Holder::Invitation(&invitation.id)
            .replace(&mut tx, grants)
            .await?;
        let (id, expires) = (&invitation.id, Expiry(Some(invitation.expires_at)));
        let change = format!("invitation {id:?} grants ({grants}) expires {expires}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Requires `accounts:manage` and every grant an invitation hands out.
    fn may_invite(authority: &Grants, grants: &Grants) -> Result<(), StoreError> {
        authority
            .require(Permission::Global(GlobalPermission::AccountsManage))
            .and_then(|()| authority.may_grant(grants))
            .map_err(StoreError::Denied)
    }

    /// Stores an enrollment invitation that adds a passkey to `user_id`, if
    /// `caller` may change that account and recently signed in with a passkey.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn create_enrollment(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        invitation: &NewInvitation,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let caller = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let account = Actor::Account(caller);
        if let Some(authority) = Self::check_account_on(&mut tx, account, user_id).await? {
            authority.require_unscoped()?;
            authority.require_fresh()?;
        }
        let (issuer, credential) = (caller.user_id, caller.credential_id);
        Self::insert_invitation_on(&mut tx, invitation, issuer, credential, Some(user_id)).await?;
        let (id, expires) = (&invitation.id, Expiry(Some(invitation.expires_at)));
        let change = format!("enrollment {id:?} for account {user_id:?} expires {expires}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Inserts an invitation row issued by the account `issuer` through its
    /// `credential`, targeting `user_id` for enrollment.
    async fn insert_invitation_on(
        db: &mut SqliteConnection,
        invitation: &NewInvitation,
        issuer: &str,
        credential: &str,
        user_id: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO auth_invitations(id,issuer_id,secret_hash,expires_at,user_id) VALUES(?1,?2,?3,?4,?5)",
            invitation.id,
            issuer,
            invitation.secret_hash,
            invitation.expires_at,
            user_id
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::constraint)?;
        let issuer = Issued::Credential(credential);
        Issued::Invitation(&invitation.id)
            .record(db, None, Some(issuer))
            .await
    }

    /// Deletes an unused invitation if `actor` could have created it: it may
    /// change the enrollment target, or hand out the invitation's grants.
    /// Unknown IDs are `NotFound`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn revoke_invitation(
        &self,
        author: impl Into<Author<'_>>,
        id: &str,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let target = sqlx::query_scalar!("SELECT user_id FROM auth_invitations WHERE id=?1", id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let change = match target {
            None => return Err(StoreError::NotFound),
            Some(Some(user_id)) => {
                Self::check_account_on(&mut tx, actor, &user_id).await?;
                format!("enrollment {id:?} for account {user_id:?} revoked")
            }
            Some(None) => {
                if let Some(authority) = actor.load(&mut tx).await? {
                    let grants = Holder::Invitation(id).grants(&mut tx).await?;
                    Self::may_invite(&authority.grants, &grants)?;
                }
                format!("invitation {id:?} revoked")
            }
        };
        sqlx::query!("DELETE FROM auth_invitations WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Stores an API token for the caller's own account, limited to grants the
    /// caller holds and optionally bound to a tailnet user or tag. Scoped
    /// callers cannot create tokens, and others need a recent passkey sign-in
    /// (see [`Authority::require_fresh`]).
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn create_token(
        &self,
        author: impl Into<Author<'_, Caller<'_>>>,
        credential: &NewCredential<'_>,
        tailnet: Option<&TailnetBinding>,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let caller = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let authority = caller.load(&mut tx).await?;
        authority.may_hand_out(credential.grants)?;
        authority.require_fresh()?;
        let issuer = Issued::Credential(caller.credential_id);
        credential
            .insert(&mut tx, &authority.user_id, Some(issuer))
            .await?;
        let tailnet = tailnet.map(TailnetBinding::as_str);
        sqlx::query!(
            "UPDATE auth_credentials SET tailnet=?1 WHERE id=?2",
            tailnet,
            credential.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let actor = Actor::Account(caller).attribution();
        Self::check_token_on(&mut tx, credential, actor).await?;
        let change = credential.describe(tailnet);
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Deletes auth state that can no longer be used: expired credentials
    /// and browser sessions idle for a day, with their grants, and expired
    /// invitations, admin recovery links, and host operator links and
    /// sessions. It then forgets the issuance lineage nothing live needs
    /// any more. Long-lived connections recheck their access when a
    /// credential went.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_auth(&self) -> Result<(), StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let credentials = sqlx::query!(
            "DELETE FROM auth_credentials WHERE expires_at<=?1 OR (kind='browser' AND last_used_at<=?2)",
            now,
            idle
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        sqlx::query!("DELETE FROM auth_invitations WHERE expires_at<=?1", now)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM auth_recovery WHERE expires_at<=?1", now)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM auth_operator_sessions WHERE expires_at<=?1",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        Self::forget_lineage_on(&mut tx).await?;
        if credentials > 0 {
            self.commit_access_change(tx, Actor::Daemon, None).await
        } else {
            tx.commit().await.map_err(StoreError::database)
        }
    }
}

/// Fixtures for authentication tests that need states no API produces.
#[cfg(any(test, feature = "testing"))]
impl Store {
    /// Claims the installation for an account with `grants` and no passkey.
    ///
    /// # Panics
    /// If the store fails.
    pub async fn seed_auth_user(&self, user: &User, grants: &Grants) {
        let (_writer, mut tx) = self.begin_immediate().await.unwrap();
        Self::insert_user_on(&mut tx, user, now_secs())
            .await
            .unwrap();
        Holder::User(&user.id)
            .replace(&mut tx, grants)
            .await
            .unwrap();
        sqlx::query!("UPDATE auth_setup SET initialized=1,secret_hash=NULL")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// Stores a credential for an existing account, as a lineage root.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn insert_credential(
        &self,
        user_id: &str,
        credential: &NewCredential<'_>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        credential.insert(&mut tx, user_id, None).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Holds the writer queue, as a long write would, until the guard drops.
    pub async fn hold_writer(&self) -> crate::Writer<'_> {
        self.writers.lock().await
    }

    /// Waits until another writer queues behind the turn the caller holds,
    /// such as with [`Store::hold_writer`].
    pub async fn writer_waiting(&self) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the store owns the sender, so the wait ends only with a writer"
        )]
        let _ = self
            .writers
            .waiting
            .subscribe()
            .wait_for(|waiting| *waiting > 0)
            .await;
    }

    /// Makes every credential look unused since `secs` ago.
    ///
    /// # Panics
    /// If the store fails.
    pub async fn age_auth_credentials(&self, secs: i64) {
        let last_used = now_secs() - secs;
        self.write(sqlx::query!(
            "UPDATE auth_credentials SET last_used_at=?1",
            last_used
        ))
        .await
        .unwrap();
    }

    /// Makes every host operator link and session expire now.
    ///
    /// # Panics
    /// If the store fails.
    pub async fn expire_operator_sessions(&self) {
        let now = now_secs();
        self.write(sqlx::query!(
            "UPDATE auth_operator_sessions SET expires_at=?1",
            now
        ))
        .await
        .unwrap();
    }

    /// Empties the account table behind the daemon's back.
    ///
    /// # Panics
    /// If the store fails.
    pub async fn clear_auth_users(&self) {
        self.write(sqlx::query!("DELETE FROM auth_users"))
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests;
