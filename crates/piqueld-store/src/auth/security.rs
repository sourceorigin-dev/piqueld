//! Security events: daemon history about access to the daemon itself. Each
//! is delivered as a `security` notification (see `process_notification_event`).
use crate::auth::access::Holder;
use crate::{Attribution, NewCredential, Store, StoreError, now_ms};
use piqueld_core::{EventKind, access::Grants};
use sqlx::SqliteConnection;

/// A security-relevant occurrence, by its event kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityEvent {
    /// An account received `admin` on every application.
    AdminGranted,
    /// A token was created without expiry or with `admin`.
    PrivilegedToken,
    /// Many requests from one address and account were refused in a minute.
    /// The event's `resource` is that address (absent for the Unix socket).
    DenialBurst,
    /// An admin recovery link was issued over the Unix socket.
    RecoveryIssued,
    /// A host operator sign-in link was issued over the Unix socket.
    OperatorSignInIssued,
    /// A credential was used from an address it had not used before.
    NewAddress,
    /// The secret master key was recovered, discarding stored values.
    SecretKeyRecovered,
    /// A notification destination was created, changed or deleted, which
    /// could silence notifications.
    DestinationChanged,
}

impl SecurityEvent {
    /// Every security event.
    const ALL: [Self; 8] = [
        Self::AdminGranted,
        Self::PrivilegedToken,
        Self::DenialBurst,
        Self::RecoveryIssued,
        Self::OperatorSignInIssued,
        Self::NewAddress,
        Self::SecretKeyRecovered,
        Self::DestinationChanged,
    ];

    /// Event kind in daemon history.
    #[must_use]
    const fn kind(self) -> EventKind {
        match self {
            Self::AdminGranted => EventKind::AdminGranted,
            Self::PrivilegedToken => EventKind::PrivilegedTokenCreated,
            Self::DenialBurst => EventKind::AccessDenialBurst,
            Self::RecoveryIssued => EventKind::AdminRecoveryIssued,
            Self::OperatorSignInIssued => EventKind::OperatorSignInIssued,
            Self::NewAddress => EventKind::CredentialNewAddress,
            Self::SecretKeyRecovered => EventKind::SecretKeyRecovered,
            Self::DestinationChanged => EventKind::NotificationDestinationChanged,
        }
    }

    /// The security event an event kind records, if any.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| *kind == event.kind())
    }
}

/// Refused requests from one address and account within a minute that raise
/// [`SecurityEvent::DenialBurst`].
const DENIAL_BURST: i64 = 20;
/// Minimum time between [`SecurityEvent::DenialBurst`] alerts for one address
/// and account, so a sustained attack notifies periodically, not per request.
const DENIAL_BURST_COOLDOWN_MS: i64 = 10 * 60_000;

impl Store {
    /// Records `event` in daemon history, caused by `actor`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn security_event_on(
        db: &mut SqliteConnection,
        event: SecurityEvent,
        message: &str,
        actor: Attribution<'_>,
    ) -> Result<(), StoreError> {
        let kind = event.kind().as_str();
        let now = now_ms();
        let operator = actor.operator_uid();
        let system = actor.system.map(piqueld_core::sync::SystemActor::as_str);
        sqlx::query!(
            "INSERT INTO events(scope,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid,actor_system)
            VALUES('daemon',?1,?2,?3,?4,?5,?6,?7)",
            kind,
            message,
            now,
            actor.user_id,
            actor.credential_id,
            operator,
            system,
        )
        .execute(db)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Replaces `user_id`'s grants on behalf of `actor`, raising
    /// [`SecurityEvent::AdminGranted`] when the account newly holds `admin`
    /// on every application. `how` completes "became an administrator …".
    ///
    /// # Errors
    /// Returns a storage error.
    pub(super) async fn replace_user_grants_on(
        db: &mut SqliteConnection,
        user_id: &str,
        grants: &Grants,
        actor: Attribution<'_>,
        how: &str,
    ) -> Result<(), StoreError> {
        let was_admin = Holder::User(user_id).grants(&mut *db).await?.is_superuser();
        Holder::User(user_id).replace(&mut *db, grants).await?;
        if grants.is_superuser() && !was_admin {
            let username = Self::username_on(db, user_id).await?;
            let message = format!("{username} became an administrator {how}");
            Self::security_event_on(db, SecurityEvent::AdminGranted, &message, actor).await?;
        }
        Ok(())
    }

    /// Raises [`SecurityEvent::PrivilegedToken`] for a new API token that
    /// never expires or holds `admin`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(super) async fn check_token_on(
        db: &mut SqliteConnection,
        credential: &NewCredential<'_>,
        actor: Attribution<'_>,
    ) -> Result<(), StoreError> {
        let admin = credential
            .grants
            .is_some_and(|grants| !grants.admin_scope().is_empty());
        let never_expires = credential.expires_at.is_none();
        if !(admin || never_expires) {
            return Ok(());
        }
        let username = match actor.user_id {
            Some(user_id) => Self::username_on(db, user_id).await?,
            None => "The daemon".to_owned(),
        };
        let detail = match (admin, never_expires) {
            (true, true) => "with admin and no expiry",
            (true, false) => "with admin",
            _ => "with no expiry",
        };
        let message = format!(
            "{username} created API token \"{}\" {detail}",
            credential.name
        );
        Self::security_event_on(db, SecurityEvent::PrivilegedToken, &message, actor).await
    }

    /// An account's username.
    pub(super) async fn username_on(
        db: &mut SqliteConnection,
        user_id: &str,
    ) -> Result<String, StoreError> {
        sqlx::query_scalar!("SELECT username FROM auth_users WHERE id=?1", user_id)
            .fetch_one(db)
            .await
            .map_err(StoreError::database)
    }

    /// Raises [`SecurityEvent::DenialBurst`] when a just-recorded refusal
    /// brings its address and account to [`DENIAL_BURST`] refusals within a
    /// minute, unless they raised one within [`DENIAL_BURST_COOLDOWN_MS`].
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn check_denial_burst_on(
        db: &mut SqliteConnection,
        peer: Option<&str>,
        actor: Attribution<'_>,
        username: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        let (window, cooldown) = (now - 60_000, now - DENIAL_BURST_COOLDOWN_MS);
        let operator = actor.operator_uid();
        let event_kind = EventKind::AccessDenialBurst.as_str();
        let burst = sqlx::query_scalar!(
            r#"SELECT NOT EXISTS(SELECT 1 FROM events
                WHERE kind=?7 AND resource IS ?2 AND actor_user_id IS ?3
                AND actor_operator_uid IS ?6 AND created_at_ms>=?5)
            AND (SELECT COUNT(*) FROM (SELECT 1 FROM audit_events
                WHERE outcome='denied' AND user_id IS ?3 AND peer IS ?2
                AND operator_uid IS ?6 AND created_at_ms>=?1
                LIMIT ?4)) >= ?4
            AS "burst!: bool""#,
            window,
            peer,
            actor.user_id,
            DENIAL_BURST,
            cooldown,
            operator,
            event_kind,
        )
        .fetch_one(&mut *db)
        .await
        .map_err(StoreError::database)?;
        if !burst {
            return Ok(());
        }
        let message = format!(
            "{DENIAL_BURST} requests refused within a minute from {} as {}",
            peer.unwrap_or("the Unix socket"),
            username.map_or_else(
                || actor.operator.map_or_else(
                    || "an anonymous caller".to_owned(),
                    |operator| operator.to_string()
                ),
                str::to_owned,
            ),
        );
        Self::security_event_on(db, SecurityEvent::DenialBurst, &message, actor).await?;
        sqlx::query!(
            "UPDATE events SET resource=?1 WHERE id=last_insert_rowid()",
            peer
        )
        .execute(db)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Notes that `credential_id`, owned by `user_id`, was used from
    /// `address`, raising [`SecurityEvent::NewAddress`] the first time a
    /// credential already used elsewhere appears there. Unknown credentials
    /// are ignored.
    /// # Errors
    /// Returns storage errors.
    pub async fn note_credential_address(
        &self,
        credential_id: &str,
        user_id: &str,
        address: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let added = sqlx::query!(
            "INSERT OR IGNORE INTO auth_credential_addresses(credential_id,address,first_seen_ms)
            SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM auth_credentials WHERE id=?1)",
            credential_id,
            address,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if added == 1 {
            let known = sqlx::query!(
                r#"SELECT c.name,c.kind,u.username,
                (SELECT COUNT(*) FROM auth_credential_addresses WHERE credential_id=c.id) AS "addresses!: i64"
                FROM auth_credentials c JOIN auth_users u ON u.id=c.user_id WHERE c.id=?1"#,
                credential_id,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if known.addresses > 1 {
                let message = format!(
                    "{}'s {} \"{}\" was used from a new address: {address}",
                    known.username, known.kind, known.name
                );
                let actor = Attribution {
                    user_id: Some(user_id),
                    credential_id: Some(credential_id),
                    ..Attribution::default()
                };
                Self::security_event_on(&mut tx, SecurityEvent::NewAddress, &message, actor)
                    .await?;
            }
        }
        tx.commit().await.map_err(StoreError::database)
    }
}
