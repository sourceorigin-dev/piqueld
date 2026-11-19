//! The audit trail: one row per audited API request, newest first. Rows copy
//! account and credential details, so they outlive both, and survive
//! application deletion under their own retention.
//!
//! Rows form a hash chain: they have consecutive IDs, and each stores a
//! `link`, the SHA-256 of its predecessor's link and its own ID and fields
//! (see [`AuditRow::link_from`]). The oldest retained row extends the newest
//! pruned one, `audit_chain.pruned_through` with `pruned_link`.
use super::{Actor, Attribution, Paging, Store, StoreError, days_ago_ms, now_ms};
use piqueld_core::api::Page;
use piqueld_core::audit::{AuditEvent, AuditFilter, AuditLink, AuditOutcome, AuditVerification};
use piqueld_core::auth::HostOperator;
use sqlx::{QueryBuilder, SqliteConnection};

/// Columns read into [`AuditRow`].
const COLUMNS: &str = "id,created_at_ms,action,outcome,status,user_id,username,credential_id,\
    credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link,tailnet,operator_uid";

/// Most audit rows one pruning transaction deletes, so that pruning a large
/// backlog never holds the writer for long.
const AUDIT_PRUNE_BATCH: i64 = 1_000;

/// An audited request to record.
pub struct NewAuditEvent {
    /// Method and route template, then what the request changed beyond it.
    pub action: String,
    /// Whether it succeeded, was denied, or failed.
    pub outcome: AuditOutcome,
    /// HTTP status of the response, or of the change it records (see
    /// [`AuditRequest`]).
    pub status: u16,
    /// Who made the request and from where.
    pub origin: AuditOrigin,
    /// Application the route names by ID.
    pub application_id: Option<String>,
    /// Environment the route names by ID; its application is recorded too.
    pub environment_id: Option<String>,
    /// The permission the route requires.
    pub permission: Option<&'static str>,
}

/// Who made an API request and from where, as its audit records name them.
#[derive(Clone, Debug, Default)]
pub struct AuditOrigin {
    /// The signed-in account's ID.
    pub user_id: Option<String>,
    /// The signed-in account's name.
    pub username: Option<String>,
    /// The credential the request was signed in with.
    pub credential_id: Option<String>,
    /// That credential's kind, such as a token or a browser session.
    pub credential_kind: Option<&'static str>,
    /// Whether that credential is limited to some applications.
    pub scoped: Option<bool>,
    /// The peer's network address.
    pub peer: Option<String>,
    /// Tailnet identity of the peer (see `TailnetPeer::describe`).
    pub tailnet: Option<String>,
    /// The request's ID, as its response reports it.
    pub request_id: Option<String>,
    /// The host operator, when it made the request instead of an account.
    pub operator: Option<HostOperator>,
}

/// An API request whose changes record themselves: each change it commits
/// appends its own `allowed` record in the change's transaction (see
/// [`Store::append_audit_on`]), so a committed change always has one. Its
/// status is the one the API answers that change with, e.g. 202 for an
/// accepted deployment, even if the response later fails. A request that
/// commits nothing, or whose response is an error, is still recorded in
/// the background like any other.
#[derive(Clone, Debug)]
pub struct AuditRequest {
    /// Method and route template, e.g. `POST /api/v1/applications`.
    pub route: String,
    /// Who made it and from where.
    pub origin: AuditOrigin,
    /// The status its route answers every change with, when that doesn't
    /// depend on the change, e.g. 200 for a batch of deletions.
    pub status: Option<u16>,
    /// Set once a change it made committed with its record.
    pub recorded: Recorded,
}

/// Whether a request already recorded a change it committed (see
/// [`AuditRequest`]), so its success needs no background record. Clones
/// share the flag.
#[derive(Clone, Debug, Default)]
pub struct Recorded(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Recorded {
    /// Whether a change was recorded.
    #[must_use]
    pub fn get(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Notes that a change committed with its record.
    fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl AuditRequest {
    /// Its record with `outcome` and `status`, naming `detail` after its
    /// route when there is one.
    #[must_use]
    pub fn event(&self, outcome: AuditOutcome, status: u16, detail: Option<&str>) -> NewAuditEvent {
        let mut action = self.route.clone();
        if let Some(detail) = detail {
            action.push(' ');
            action.push_str(detail);
        }
        NewAuditEvent {
            action,
            outcome,
            status,
            origin: self.origin.clone(),
            application_id: None,
            environment_id: None,
            permission: None,
        }
    }
}

/// Who submits a write, and the API request it comes from, if any: changes
/// a request makes record themselves (see [`AuditRequest`]). An [`Actor`]
/// alone, like the daemon's own work, records none.
#[derive(Clone, Copy, Debug)]
pub struct Author<'a> {
    /// Who acts, re-read inside the writing transaction.
    pub actor: Actor<'a>,
    /// The request it acts through.
    pub request: Option<&'a AuditRequest>,
}

impl<'a> From<Actor<'a>> for Author<'a> {
    fn from(actor: Actor<'a>) -> Self {
        Self {
            actor,
            request: None,
        }
    }
}

impl<'a> Author<'a> {
    /// Appends to `db` the record of the `change` its request commits there,
    /// if it changed anything: `allowed`, with `status` unless its route
    /// answers another, about `application` and `environment`. Does nothing
    /// without a request or a change. Call [`Appended::committed`] on the
    /// result once the transaction commits.
    /// # Errors
    /// Returns storage errors, which roll the change back too.
    pub(crate) async fn record_on(
        self,
        db: &mut SqliteConnection,
        status: u16,
        change: Option<&str>,
        (application, environment): (Option<&str>, Option<&str>),
    ) -> Result<Appended<'a>, StoreError> {
        let (Some(request), Some(change)) = (self.request, change) else {
            return Ok(Appended(None));
        };
        let status = request.status.unwrap_or(status);
        let mut event = request.event(AuditOutcome::Allowed, status, Some(change));
        event.application_id = application.map(str::to_owned);
        event.environment_id = environment.map(str::to_owned);
        Store::append_audit_on(db, &event, now_ms()).await?;
        Ok(Appended(Some(&request.recorded)))
    }
}

/// What [`Author::record_on`] appended: the request whose change it
/// recorded, if any, to note once the transaction commits. Only an appended
/// record can mark its request recorded, so a request that changed nothing
/// keeps its background record.
#[must_use = "note the record once its transaction commits"]
pub(crate) struct Appended<'a>(Option<&'a Recorded>);

impl Appended<'_> {
    /// Notes that the recorded change committed (see [`Recorded`]).
    pub(crate) fn committed(self) {
        if let Some(recorded) = self.0 {
            recorded.set();
        }
    }
}

/// Raw `audit_events` row.
#[derive(sqlx::FromRow)]
struct AuditRow {
    id: i64,
    created_at_ms: i64,
    action: String,
    outcome: String,
    status: i64,
    user_id: Option<String>,
    username: Option<String>,
    credential_id: Option<String>,
    credential_kind: Option<String>,
    scoped: Option<bool>,
    peer: Option<String>,
    request_id: Option<String>,
    application_id: Option<String>,
    environment_id: Option<String>,
    permission: Option<String>,
    link: Option<String>,
    tailnet: Option<String>,
    operator_uid: Option<i64>,
}

impl NewAuditEvent {
    /// Marks the place of `count` earlier records that could not be written,
    /// because the backlog was full or storage failed, so the trail shows
    /// the gap instead of hiding it: a `failed` record with status 503 and
    /// no caller.
    #[must_use]
    pub fn lost(count: u64) -> Self {
        Self {
            action: format!("audit records missing before this one: {count}"),
            outcome: AuditOutcome::Failed,
            status: 503,
            origin: AuditOrigin::default(),
            application_id: None,
            environment_id: None,
            permission: None,
        }
    }

    /// The row to store as `id` at `created_at_ms`, before its link exists.
    fn row(&self, id: i64, created_at_ms: i64) -> AuditRow {
        AuditRow {
            id,
            created_at_ms,
            action: self.action.clone(),
            outcome: self.outcome.as_str().to_owned(),
            status: i64::from(self.status),
            user_id: self.origin.user_id.clone(),
            username: self.origin.username.clone(),
            credential_id: self.origin.credential_id.clone(),
            credential_kind: self.origin.credential_kind.map(str::to_owned),
            scoped: self.origin.scoped,
            peer: self.origin.peer.clone(),
            request_id: self.origin.request_id.clone(),
            application_id: self.application_id.clone(),
            environment_id: self.environment_id.clone(),
            permission: self.permission.map(str::to_owned),
            link: None,
            tailnet: self.origin.tailnet.clone(),
            operator_uid: self.origin.operator.map(|operator| i64::from(operator.uid)),
        }
    }
}

impl AuditRow {
    /// This row's link after `previous`: lowercase hex SHA-256 of a JSON
    /// array holding `previous` and every field, in column order. `tailnet`
    /// and `operator_uid`, added later, are appended only when present, so
    /// links of earlier rows stay valid: `[fields, tailnet]` with a tailnet
    /// identity alone, `[fields, tailnet, operator_uid]` (`tailnet` possibly
    /// null) with an operator.
    fn link_from(&self, previous: &str) -> Result<String, StoreError> {
        use sha2::{Digest, Sha256};
        let fields = (
            previous,
            self.id,
            self.created_at_ms,
            &self.action,
            &self.outcome,
            self.status,
            &self.user_id,
            &self.username,
            &self.credential_id,
            &self.credential_kind,
            self.scoped,
            &self.peer,
            &self.request_id,
            &self.application_id,
            &self.environment_id,
            &self.permission,
        );
        let bytes = match (&self.tailnet, self.operator_uid) {
            (None, None) => serde_json::to_vec(&fields),
            (Some(tailnet), None) => serde_json::to_vec(&(fields, tailnet)),
            (tailnet, Some(operator)) => serde_json::to_vec(&(fields, tailnet, operator)),
        }
        .map_err(StoreError::corrupt)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn into_event(self) -> Result<AuditEvent, StoreError> {
        Ok(AuditEvent {
            id: self.id,
            created_at_ms: self.created_at_ms,
            action: self.action,
            outcome: AuditOutcome::parse(&self.outcome).ok_or(StoreError::Corrupt)?,
            status: u16::try_from(self.status).map_err(StoreError::corrupt)?,
            user_id: self.user_id,
            username: self.username,
            credential_id: self.credential_id,
            credential_kind: self.credential_kind,
            scoped: self.scoped,
            peer: self.peer,
            tailnet: self.tailnet,
            request_id: self.request_id,
            application_id: self.application_id,
            environment_id: self.environment_id,
            permission: self.permission,
            operator: self.operator_uid.map(Store::host_operator).transpose()?,
        })
    }
}

impl Store {
    /// Records one audited request, linked to the newest record, and raises
    /// a security event when it completes a burst of refusals. A request about
    /// an environment also records the environment's application, while it
    /// exists, if the caller may know it: when the request was allowed, or
    /// refused for a missing permission, which only happens on applications
    /// the caller can read. Callers read their own trail and must not learn
    /// which application owns an environment hidden from them.
    /// # Errors
    /// Returns storage errors.
    pub async fn record_audit(&self, event: &NewAuditEvent) -> Result<(), StoreError> {
        self.record_audit_at(event, now_ms()).await
    }

    /// [`Store::record_audit`] at `created_at_ms`.
    pub(super) async fn record_audit_at(
        &self,
        event: &NewAuditEvent,
        created_at_ms: i64,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::append_audit_on(&mut tx, event, created_at_ms).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Appends `event` to the chain inside the caller's write transaction
    /// (see [`Store::record_audit`]), so the record commits or rolls back
    /// with the change it describes. It neither begins nor commits: the
    /// caller holds the writer, which orders appends from every
    /// transaction into one chain.
    /// # Errors
    /// Returns storage errors, which roll back the caller's change with it.
    pub(crate) async fn append_audit_on(
        db: &mut SqliteConnection,
        event: &NewAuditEvent,
        created_at_ms: i64,
    ) -> Result<(), StoreError> {
        let head = Self::audit_head_on(&mut *db).await?;
        // Only a tampered trail reaches the highest possible ID.
        let id = head.id.checked_add(1).ok_or(StoreError::Corrupt)?;
        let mut row = event.row(id, created_at_ms);
        if row.application_id.is_none()
            && (row.outcome == AuditOutcome::Allowed.as_str() || row.permission.is_some())
            && let Some(environment) = &row.environment_id
        {
            row.application_id = sqlx::query_scalar!(
                r#"SELECT application_id AS "application_id!" FROM environments WHERE id=?1"#,
                environment
            )
            .fetch_optional(&mut *db)
            .await
            .map_err(StoreError::database)?;
        }
        row.link = Some(row.link_from(&head.link)?);
        sqlx::query!(
            "INSERT INTO audit_events(created_at_ms,action,outcome,status,user_id,username,credential_id,credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link,id,tailnet,operator_uid) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            row.created_at_ms,
            row.action,
            row.outcome,
            row.status,
            row.user_id,
            row.username,
            row.credential_id,
            row.credential_kind,
            row.scoped,
            row.peer,
            row.request_id,
            row.application_id,
            row.environment_id,
            row.permission,
            row.link,
            row.id,
            row.tailnet,
            row.operator_uid,
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?;
        if event.outcome == AuditOutcome::Denied {
            let actor = Attribution {
                user_id: row.user_id.as_deref(),
                credential_id: row.credential_id.as_deref(),
                operator: event.origin.operator,
                ..Attribution::default()
            };
            let (peer, username) = (row.peer.as_deref(), row.username.as_deref());
            Self::check_denial_burst_on(db, peer, actor, username, row.created_at_ms).await?;
        }
        Ok(())
    }

    /// The record a new one extends: the newest record, else the anchor,
    /// else the chain's empty start before ID 1.
    /// # Errors
    /// Returns [`StoreError::Corrupt`] for a half-stored anchor.
    async fn audit_head_on(db: &mut SqliteConnection) -> Result<AuditLink, StoreError> {
        let anchor = Self::audit_anchor_on(&mut *db).await?;
        let newest = sqlx::query!(
            r#"SELECT id AS "id!",COALESCE(link,'') AS "link!: String" FROM audit_events
            ORDER BY id DESC LIMIT 1"#
        )
        .fetch_optional(db)
        .await
        .map_err(StoreError::database)?;
        Ok(newest.map_or_else(
            || anchor.unwrap_or_else(Self::audit_start),
            |row| AuditLink {
                id: row.id,
                link: row.link,
            },
        ))
    }

    /// The newest pruned record, which the oldest retained one extends;
    /// `None` until something is pruned.
    /// # Errors
    /// Returns [`StoreError::Corrupt`] when only its ID or its link is stored,
    /// rather than silently verifying from the start.
    async fn audit_anchor_on(db: &mut SqliteConnection) -> Result<Option<AuditLink>, StoreError> {
        let chain = sqlx::query!("SELECT pruned_through,pruned_link FROM audit_chain")
            .fetch_one(db)
            .await
            .map_err(StoreError::database)?;
        match (chain.pruned_through, chain.pruned_link) {
            (Some(id), Some(link)) => Ok(Some(AuditLink { id, link })),
            (None, None) => Ok(None),
            _ => Err(StoreError::Corrupt),
        }
    }

    /// What the first record extends: nothing, before ID 1.
    fn audit_start() -> AuditLink {
        AuditLink {
            id: 0,
            link: String::new(),
        }
    }

    /// Recomputes every retained record's link in order, inside one read
    /// transaction so concurrent pruning cannot interleave.
    /// # Errors
    /// Returns storage errors, or [`StoreError::Corrupt`] for a half-stored
    /// anchor.
    pub async fn verify_audit(&self) -> Result<AuditVerification, StoreError> {
        let mut tx = self.snapshot().await?;
        Self::verify_audit_on(&mut tx, i64::MAX).await
    }

    /// Checks records from the pruning anchor through ID `through`: each must
    /// have the next ID and the link that ID, its fields, and its predecessor
    /// give.
    async fn verify_audit_on(
        db: &mut SqliteConnection,
        through: i64,
    ) -> Result<AuditVerification, StoreError> {
        let anchor = Self::audit_anchor_on(&mut *db).await?;
        let mut previous = anchor.clone().unwrap_or_else(Self::audit_start);
        let mut result = AuditVerification {
            checked: 0,
            anchor,
            head: None,
            broken_at: None,
        };
        // Every row, including any at the lowest possible ID, in pages that
        // seek to the inclusive cursor `from`.
        let page = format!(
            "SELECT {COLUMNS} FROM audit_events WHERE id >= ?1 AND id <= ?2 ORDER BY id LIMIT 500"
        );
        let mut from = i64::MIN;
        loop {
            let rows = sqlx::query_as::<_, AuditRow>(&page)
                .bind(from)
                .bind(through)
                .fetch_all(&mut *db)
                .await
                .map_err(StoreError::database)?;
            let Some(last) = rows.last() else {
                return Ok(result);
            };
            // The highest possible ID ends the trail.
            let next = last.id.checked_add(1);
            for row in rows {
                let expected = row.link_from(&previous.link)?;
                if previous.id.checked_add(1) != Some(row.id)
                    || row.link.as_ref() != Some(&expected)
                {
                    result.broken_at = Some(row.id);
                    return Ok(result);
                }
                result.checked += 1;
                previous = AuditLink {
                    id: row.id,
                    link: expected,
                };
                result.head = Some(previous.clone());
            }
            let Some(next) = next else {
                return Ok(result);
            };
            from = next;
        }
    }

    /// Reads audited requests matching `filter`, newest first, before the
    /// opaque `v1:<id>` cursor.
    /// # Errors
    /// Returns invalid cursor or storage errors.
    pub async fn audit_events(
        &self,
        filter: &AuditFilter,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<AuditEvent>, StoreError> {
        let paging = Paging::new(limit)?;
        let before = cursor
            .map(Self::event_cursor)
            .transpose()?
            .unwrap_or(i64::MAX);
        let mut query =
            QueryBuilder::new(format!("SELECT {COLUMNS} FROM audit_events WHERE id < "));
        query.push_bind(before);
        // Usernames compare case-insensitively, like accounts' own.
        for (column, value, collation) in [
            ("user_id", filter.user_id.as_deref(), ""),
            ("username", filter.username.as_deref(), " COLLATE NOCASE"),
            ("credential_id", filter.credential_id.as_deref(), ""),
            ("outcome", filter.outcome.map(AuditOutcome::as_str), ""),
        ] {
            if let Some(value) = value {
                query
                    .push(" AND ")
                    .push(column)
                    .push(" = ")
                    .push_bind(value)
                    .push(collation);
            }
        }
        query
            .push(" ORDER BY id DESC LIMIT ")
            .push_bind(paging.fetch);
        let mut rows = query
            .build_query_as::<AuditRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.id);
        let items = rows
            .into_iter()
            .map(AuditRow::into_event)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_cursor })
    }

    /// Prunes the oldest audit rows past the configured retention, keeping
    /// the newest pruned one as the chain's new anchor. Only rows before the
    /// oldest retained one are pruned, so one recorded after the clock
    /// stepped back waits until every row before it expired. Zero days
    /// disables pruning. Rows are only pruned while the chain through them
    /// verifies, so pruning never erases evidence of tampering; a broken
    /// chain is logged and left alone, without stopping other pruning.
    /// # Errors
    /// Returns storage errors.
    pub async fn prune_audit(&self) -> Result<(), StoreError> {
        if self.audit_days == 0 {
            return Ok(());
        }
        let cutoff = days_ago_ms(self.audit_days);
        while self.prune_audit_batch(cutoff).await? {}
        Ok(())
    }

    /// Prunes up to [`AUDIT_PRUNE_BATCH`] of the rows [`Self::prune_audit`]
    /// prunes, those before the oldest one created at or after `cutoff`,
    /// returning whether it pruned any.
    async fn prune_audit_batch(&self, cutoff: i64) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let newest = sqlx::query_scalar!(
            r#"SELECT MAX(id) AS "id: i64" FROM (SELECT id FROM audit_events WHERE id < IFNULL((SELECT MIN(id) FROM audit_events WHERE created_at_ms >= ?1),id+1) ORDER BY id LIMIT ?2)"#,
            cutoff,
            AUDIT_PRUNE_BATCH,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(newest) = newest else {
            return Ok(false);
        };
        let verified = Self::verify_audit_on(&mut tx, newest).await?;
        let Some(anchor) = verified.head.filter(|_| verified.broken_at.is_none()) else {
            tracing::error!(
                broken_at = verified.broken_at,
                "the audit trail is broken; it is not pruned, to keep the evidence"
            );
            return Ok(false);
        };
        sqlx::query!("DELETE FROM audit_events WHERE id <= ?1", anchor.id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE audit_chain SET pruned_through=?1,pruned_link=?2",
            anchor.id,
            anchor.link
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(true)
    }
}
