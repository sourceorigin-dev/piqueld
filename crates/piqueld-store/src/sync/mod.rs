//! What sync needs and records: the applications that sync, the branch head
//! each environment and preview last synced, each application's last branch
//! listing, and webhook secrets.
use super::acceptance::Accepted;
use super::{Author, Store, StoreError, StoredEnvironment, now_ms};
use crate::secrets::{Envelope, Generate, SecretCipher, SecretOwner};
use crate::{ListedHead, MutationResponse, SyncOutcome};
use piqueld_core::EventKind;
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::{AppPermission, Permission},
    api::{AcceptedOperation, EnvironmentView},
    manifest::{
        ApplicationTemplate, ManifestRevision, RepositoryManifest, SecretEncoding, SecretGenerator,
    },
    sync::SyncCheck,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use zeroize::Zeroizing;

/// The head of the branch an environment or preview tracks, as one of its
/// deployments found it while fetching: what sync follows from.
pub struct BranchHead {
    /// URL of the repository it was read from.
    pub url: String,
    /// The tracked branch.
    pub branch: String,
    /// Full commit hash at its head; `None` when the repository has no such
    /// branch, which only a local snapshot records (see [`SyncedHead`]).
    ///
    /// [`SyncedHead`]: piqueld_core::sync::SyncedHead
    pub commit: Option<String>,
}

/// An application that syncs, with everything following its pushes.
pub struct SyncApplication {
    /// Stable application ID.
    pub id: ApplicationId,
    /// Its repository connection, whose `sync` is not off.
    pub connection: RepositoryManifest,
    /// Its environments and previews that follow pushes.
    pub following: Vec<EnvironmentView>,
}

impl Store {
    /// Lists every application that syncs and is not being deleted, with the
    /// environments and previews that follow its pushes.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn sync_applications(&self) -> Result<Vec<SyncApplication>, StoreError> {
        let mut connection = self.connection().await?;
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",desired_json FROM applications WHERE delete_intent=0 AND json_extract(desired_json,'$.spec.manifest.sync.mode') IS NOT NULL ORDER BY id"#
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        let mut applications = Vec::new();
        for row in rows {
            let template: ApplicationTemplate =
                serde_json::from_str(&row.desired_json).map_err(StoreError::corrupt)?;
            let Some(connection_settings) = template.spec().manifest.clone() else {
                continue;
            };
            if connection_settings.sync.is_off() {
                continue;
            }
            let following = Self::deployables_on(&mut connection, &row.id)
                .await?
                .into_iter()
                .filter(|deployable| {
                    !deployable.delete_intent
                        && deployable.sync_state(Some(&connection_settings)).listed()
                })
                .collect();
            applications.push(SyncApplication {
                id: template.id().clone(),
                connection: connection_settings,
                following,
            });
        }
        Ok(applications)
    }

    /// Deploys `head`, the commit sync found at `branch` of `repository`, to
    /// environment or preview `id` (see [`ListedHead`]), once the branch moved past
    /// its last deployment's head:
    /// 1. `Current` when it doesn't follow that branch of that repository
    ///    (re-pointed, pinned, not opted in, stopped, sync off, being deleted).
    /// 2. `Busy` while a deployment runs without failing, or once one recorded a
    ///    head since the listing started; the caller retries, so pushes coalesce.
    /// 3. `Current` for its last deployment's head, or anything before its first.
    /// 4. Otherwise deploys exactly `head` and records it: `Deployed`, waking the
    ///    controller.
    pub(super) async fn sync_on(
        tx: &mut Transaction<'_, Sqlite>,
        listed: ListedHead,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let outcome = Self::sync_outcome_on(tx, &listed, now).await?;
        Ok(Accepted {
            changed: true,
            wake: matches!(outcome, SyncOutcome::Deployed(_)),
            response: MutationResponse::Synced(outcome),
            environments: vec![listed.id],
        })
    }

    /// What [`Self::sync_on`] does with `head`.
    async fn sync_outcome_on(
        tx: &mut Transaction<'_, Sqlite>,
        listed: &ListedHead,
        now: i64,
    ) -> Result<SyncOutcome, StoreError> {
        let ListedHead {
            id,
            repository,
            branch,
            head,
            since,
        } = listed;
        let (repository, head) = (repository.as_str(), head.as_str());
        let current = Self::existing_environment_on(tx, id.as_str()).await?;
        let connection = current.application.application.spec().manifest.as_ref();
        let following = !current.delete_intent()
            && current.environment.sync_state(connection).listed()
            && connection.is_some_and(|connection| connection.repository.url == repository)
            && current
                .environment
                .source
                .branch()
                .is_some_and(|tracked| tracked.branch() == branch.as_str());
        if !following {
            return Ok(SyncOutcome::Current);
        }
        if current.environment.synced != *since
            || Self::latest_operation_on(tx, id)
                .await?
                .is_some_and(|operation| {
                    !operation.state.terminal() && operation.error_code.is_none()
                })
        {
            return Ok(SyncOutcome::Busy);
        }
        if current
            .environment
            .synced
            .as_ref()
            .is_none_or(|synced| synced.commit.as_deref() == Some(head))
        {
            return Ok(SyncOutcome::Current);
        }
        let operation = Self::deploy_revision_on(
            tx,
            &current,
            Some(&ManifestRevision::Commit(head.to_owned())),
        )
        .await?;
        let message = format!("branch {branch} moved to {head}");
        Self::operation_event(
            tx,
            &operation.id,
            EventKind::BranchSynced,
            Some(&message),
            now,
        )
        .await?;
        Self::record_synced_on(tx, id, Some(head), now).await?;
        Ok(SyncOutcome::Deployed(AcceptedOperation::from(&operation)))
    }

    /// Starts a deployment of what a tracking environment or preview builds,
    /// at `revision` when given: requests it (see `request_deploy_on`) and
    /// captures its candidate manifest, which fetching replaces.
    /// `environment_promoted` for a promoted environment.
    pub(super) async fn deploy_revision_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        revision: Option<&ManifestRevision>,
    ) -> Result<super::Operation, StoreError> {
        let application = environment.tracking()?.candidate(revision)?;
        let operation = Self::request_deploy_on(tx, environment).await?;
        Self::insert_deployment_on(tx, &operation, &application).await?;
        Ok(operation)
    }

    /// Records `head` as the one environment or preview `id` follows from,
    /// while it still follows that branch, unpinned, of that repository. A
    /// fetch from a branch or repository it left records nothing.
    pub(super) async fn record_branch_head_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        head: &BranchHead,
        now: i64,
    ) -> Result<(), StoreError> {
        let Some(current) = Self::environment_on(tx, id.as_str()).await? else {
            return Ok(());
        };
        let repository = current
            .tracking()
            .ok()
            .and_then(|tracking| tracking.repository());
        let tracks = repository.is_some_and(|repository| {
            repository.repository.url == head.url
                && repository.repository.branch == head.branch
                && repository.repository.commit.is_none()
        });
        if tracks {
            Self::record_synced_on(tx, id, head.commit.as_deref(), now).await?;
        }
        Ok(())
    }

    /// Whether sync requested `operation`, which already recorded the head
    /// it deploys.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn requested_by_sync(
        &self,
        operation: &super::Operation,
    ) -> Result<bool, StoreError> {
        match Self::requested_by_sync_on(&mut *self.connection().await?, operation).await {
            Err(StoreError::DatabaseSource(sqlx::Error::RowNotFound)) => Ok(false),
            requested => requested,
        }
    }

    /// [`Self::requested_by_sync`] on an existing connection or transaction,
    /// where a missing operation is a storage error.
    pub(super) async fn requested_by_sync_on(
        connection: &mut SqliteConnection,
        operation: &super::Operation,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT actor_system IS NOT NULL AS "sync!: bool" FROM operations WHERE id=?1"#,
            operation.id
        )
        .fetch_one(connection)
        .await
        .map_err(StoreError::database)
    }

    /// Records `head` as the branch head of `id`'s last deployment, stored
    /// as an empty commit when the branch was absent.
    async fn record_synced_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        head: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        let (id, head) = (id.as_str(), head.unwrap_or_default());
        sqlx::query!(
            "UPDATE environments SET synced_commit=?1,synced_at_ms=?2 WHERE id=?3",
            head,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Opts environment `id` in or out of its application's sync (see
    /// `write_sync_on`) and responds with it, unless the saved manifest
    /// declares its source. Previews always sync: they are `NotFound`.
    pub(super) async fn set_sync_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: EnvironmentId,
        enabled: bool,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::checked_environment_on(tx, &id, None).await?;
        if environment.delete_intent() {
            return Err(StoreError::Busy);
        }
        Self::check_source_undeclared(
            &environment.application.application,
            &environment.environment.name,
        )?;
        let changed = Self::write_sync_on(tx, &environment.environment, enabled, now).await?;
        Ok(Self::environment_accepted(tx, id).await?.changing(changed))
    }

    /// Opts `environment` in or out of its application's sync, recording
    /// `environment_sync_changed` when that changes it, and returns whether
    /// it did. Neither changes what
    /// it deploys, so the application revision stays. Opting back in deploys
    /// the branch head at the next sync if it moved since the last deployment.
    pub(super) async fn write_sync_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &EnvironmentView,
        enabled: bool,
        now: i64,
    ) -> Result<bool, StoreError> {
        if environment.sync == enabled {
            return Ok(false);
        }
        let (row, value) = (environment.id.as_str(), i64::from(enabled));
        sqlx::query!(
            "UPDATE environments SET sync=?1,updated_at_ms=?2 WHERE id=?3",
            value,
            now,
            row
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let name = &environment.name;
        let message = if enabled {
            format!("pushes deploy environment {name} again")
        } else {
            format!("pushes no longer deploy environment {name}")
        };
        Self::environment_event_on(tx, row, EventKind::EnvironmentSyncChanged, &message, now)
            .await?;
        Ok(true)
    }

    /// Records that sync listed `application`'s branches now, and why it
    /// failed, if it did.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn record_sync_check(
        &self,
        application: &ApplicationId,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let (id, now) = (application.as_str(), now_ms());
        self.write(sqlx::query!(
            "UPDATE applications SET sync_checked_at_ms=?1,sync_error=?2 WHERE id=?3",
            now,
            error,
            id
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The last time sync listed `application`'s branches, if it did.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn sync_check(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<SyncCheck>, StoreError> {
        let id = application.as_str();
        Ok(sqlx::query!(
            "SELECT sync_checked_at_ms,sync_error FROM applications WHERE id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .and_then(|row| {
            Some(SyncCheck {
                checked_at_ms: row.sync_checked_at_ms?,
                error: row.sync_error,
            })
        }))
    }

    /// Generates a new secret for `application`'s push webhooks, replacing
    /// any previous one, and records `webhook_secret_generated`. Returns the
    /// secret, 32 random bytes as hex, and when it was generated. The
    /// author's actor needs `apps:write` on the application, checked in the
    /// transaction, which also records the request's rotation, never the
    /// secret.
    ///
    /// # Errors
    /// Returns refusal, absence, `Busy` during deletion, secret storage, or
    /// database errors.
    pub async fn generate_webhook_secret(
        &self,
        author: impl Into<Author<'_>>,
        application: &ApplicationId,
    ) -> Result<(Zeroizing<String>, i64), StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor
            .require_on(
                &mut tx,
                Permission::App(AppPermission::Write),
                Some(application),
            )
            .await?;
        let current = Self::application_on(&mut tx, application.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        if current.delete_intent {
            return Err(StoreError::Busy);
        }
        let mut generated = SecretGenerator::Random {
            bytes: 32,
            encoding: SecretEncoding::Hex,
        }
        .generate()
        .map_err(StoreError::secret)?;
        let secret = Zeroizing::new(
            String::from_utf8(std::mem::take(&mut *generated)).map_err(StoreError::corrupt)?,
        );
        let envelope = self
            .verified_secret_cipher(&mut tx)
            .await?
            .encrypt(
                SecretOwner::Webhook(application),
                "webhook",
                1,
                secret.as_bytes(),
            )
            .map_err(StoreError::secret)?;
        let (id, now) = (application.as_str(), now_ms());
        sqlx::query!(
            "INSERT INTO webhook_secrets(application_id,nonce,ciphertext,created_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(application_id) DO UPDATE SET nonce=excluded.nonce,ciphertext=excluded.ciphertext,created_at_ms=excluded.created_at_ms",
            id,
            envelope.nonce,
            envelope.ciphertext,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let by = actor.attribution();
        let operator = by.operator_uid();
        let event_kind = EventKind::WebhookSecretGenerated.as_str();
        sqlx::query!(
            "INSERT INTO events(application_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES(?1,?6,'Generated a new webhook secret; the previous one no longer verifies',?2,?3,?4,?5)",
            id,
            now,
            by.user_id,
            by.credential_id,
            operator,
            event_kind,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let change = "webhook secret generated";
        author
            .commit_recorded(tx, 200, Some(change), (Some(id), None))
            .await?;
        Ok((secret, now))
    }

    /// When `application`'s webhook secret was generated, if it has one.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn webhook_secret_created(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<i64>, StoreError> {
        let id = application.as_str();
        sqlx::query_scalar!(
            "SELECT created_at_ms FROM webhook_secrets WHERE application_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Decrypts `application`'s webhook secret, if it has one.
    ///
    /// # Errors
    /// Returns secret storage or database errors.
    pub async fn webhook_secret(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        let id = application.as_str();
        let Some(row) = sqlx::query!(
            "SELECT nonce,ciphertext FROM webhook_secrets WHERE application_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        SecretCipher::load(&self.secret_key_path, true)
            .and_then(|cipher| {
                cipher.decrypt(
                    SecretOwner::Webhook(application),
                    "webhook",
                    1,
                    &Envelope {
                        nonce: row.nonce,
                        ciphertext: row.ciphertext,
                    },
                )
            })
            .map(Some)
            .map_err(StoreError::secret)
    }
}

/// Fixtures for sync tests.
#[cfg(any(test, feature = "testing"))]
impl Store {
    /// The database, for tests reading what sync recorded.
    #[must_use]
    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    /// Deploys `id` from its source and marks the deployment succeeded,
    /// having fetched `commit` from its branch, as a finished deployment would.
    ///
    /// # Panics
    /// If the store fails.
    #[expect(
        clippy::expect_used,
        reason = "A test fixture fails the test by panicking."
    )]
    pub async fn deployed_at(&self, id: &EnvironmentId, commit: &str) -> super::Operation {
        let (writer, mut tx) = self
            .begin_immediate()
            .await
            .expect("begin immediate succeeds");
        let current = Self::environment_on(&mut tx, id.as_str())
            .await
            .expect("environment on returns a value")
            .expect("the value is present");
        let operation = Self::deploy_revision_on(&mut tx, &current, None)
            .await
            .expect("deploy revision on succeeds");
        sqlx::query(
            "UPDATE deployment_inputs SET fetched=1,repository_commit=?1 WHERE operation_id=?2",
        )
        .bind(commit)
        .bind(&operation.id)
        .execute(&mut *tx)
        .await
        .expect("execute succeeds");
        sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE id=?1")
            .bind(&operation.id)
            .execute(&mut *tx)
            .await
            .expect("execute succeeds");
        tx.commit().await.expect("commit succeeds");
        drop(writer);
        if let Some(repository) = current.tracking().expect("tracking succeeds").repository() {
            let head = BranchHead {
                url: repository.repository.url,
                branch: repository.repository.branch,
                commit: Some(commit.into()),
            };
            let (_writer, mut tx) = self
                .begin_immediate()
                .await
                .expect("begin immediate succeeds");
            Self::record_branch_head_on(&mut tx, id, &head, now_ms())
                .await
                .expect("record branch head on succeeds");
            tx.commit().await.expect("commit succeeds");
        }
        operation
    }
}

#[cfg(test)]
mod tests;
