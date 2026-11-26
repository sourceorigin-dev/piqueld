//! Atomic acceptance: compare current intent, write the change and its replay receipt together.
use super::auth::access::{EnvironmentOwner, Holder};
use super::{
    Actor, Author, Operation, OperationKind, Store, StoreError, StoredApplication, now_ms,
};
use crate::{Mutation, MutationResponse, PreviewMutation, PromotionMutation};
use piqueld_core::EventKind;
use piqueld_core::access::{AppPermission, Grants, Target};
use piqueld_core::api::{
    AcceptedOperation, CreatedPreview, DeletedApplication, EnvironmentView, SavedApplication,
};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName, EventDetail};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// A save as fingerprinted: its mutation without the inspected application
/// ID, in `Mutation::Save`'s serialized form.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename = "save")]
struct SaveIdentity<'a> {
    /// The application as saved.
    application: &'a piqueld_core::manifest::ApplicationTemplate,
    /// ID the save expected the application to have.
    expected_application_id: Option<ApplicationId>,
    /// Whether the save also deploys.
    deploy: bool,
}

/// An authorized request: a replay, or one to execute.
enum Checked {
    /// The response stored for the request it repeats.
    Replay(MutationResponse),
    /// A new request.
    New {
        /// The caller's authority; `None` for the daemon and the host operator.
        caller: Option<super::auth::access::Authority>,
        /// Whether it creates an application.
        creating: bool,
    },
}

/// A mutation's outcome inside its transaction.
pub(super) struct Accepted {
    /// Response returned to the caller and stored for replay.
    pub(super) response: MutationResponse,
    /// Whether the controller has new work.
    pub(super) wake: bool,
    /// Environments whose hostname reservations may have changed.
    pub(super) environments: Vec<EnvironmentId>,
    /// Whether it changed anything: a request that finds what it asks for
    /// already done, such as an existing preview or a pending deletion,
    /// commits no change and so records none (see `Store::accept`).
    pub(super) changed: bool,
}

impl Accepted {
    /// This outcome, which changed nothing unless `changed`.
    pub(super) fn changing(self, changed: bool) -> Self {
        Self { changed, ..self }
    }
}

impl Store {
    /// Commits a mutation and its optional replay receipt in one transaction.
    /// `expected_generation` compares the inspected application revision (zero
    /// means the application must be absent, `None` skips the check); `request_id`
    /// is the caller's idempotency key. `actor` must be allowed the mutation
    /// before any replay; an identical replay from the same account returns the
    /// stored response and never wakes the controller. `force` drops the
    /// generation and identity preconditions. Audits API changes (see
    /// [`crate::AuditRequest`]). Returns whether the controller should wake up.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn accept(
        &self,
        author: impl Into<Author<'_>>,
        mut mutation: Mutation,
        mut expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<(MutationResponse, bool), StoreError> {
        let author = author.into();
        let actor = author.actor;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let first_event =
            sqlx::query_scalar!(r#"SELECT COALESCE(MAX(id),0)+1 AS "id!: i64" FROM events"#)
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        let fingerprint = Self::mutation_fingerprint(&mutation, force)?;
        let checked =
            Self::check_request_on(&mut tx, actor, &mutation, &fingerprint, request_id, now)
                .await?;
        let (caller, creating) = match checked {
            Checked::Replay(response) => return Ok((response, false)),
            Checked::New { caller, creating } => (caller, creating),
        };
        let owner = caller.as_ref().map(|authority| authority.user_id.as_str());
        if force {
            expected_generation = None;
            if let Mutation::Save {
                expected_application_id,
                ..
            } = &mut mutation
            {
                *expected_application_id = None;
            }
        }
        let accepted = match self
            .execute_mutation(&mut tx, mutation, expected_generation, now)
            .await
        {
            Err(StoreError::PreviewLimitReached(mut reached)) => {
                if let Some(authority) = &caller {
                    let readable = authority.grants.app_scope(AppPermission::Read);
                    reached
                        .previews
                        .retain(|preview| readable.contains(&preview.application_id));
                }
                return Err(StoreError::PreviewLimitReached(reached));
            }
            accepted => accepted?,
        };
        if creating
            && let Some(authority) = caller.as_ref().filter(|authority| !authority.scoped)
            && let MutationResponse::Saved(saved) = &accepted.response
        {
            let id =
                ApplicationId::parse(saved.application_id.as_str()).map_err(StoreError::corrupt)?;
            let creator = authority.grants.for_created_application(&id);
            if !creator.is_empty() {
                Holder::User(&authority.user_id)
                    .extend(&mut tx, &creator)
                    .await?;
            }
        }
        if !matches!(actor, Actor::Daemon) {
            Self::attribute_on(&mut tx, actor.attribution(), first_event).await?;
        }
        if let Some(request_id) = request_id {
            let response_json =
                serde_json::to_string(&accepted.response).map_err(StoreError::corrupt)?;
            let expires = now.saturating_add(86_400_000);
            sqlx::query!("INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms,user_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(request_id) DO UPDATE SET fingerprint=excluded.fingerprint,response_json=excluded.response_json,expires_at_ms=excluded.expires_at_ms,user_id=excluded.user_id",request_id,fingerprint,response_json,expires,owner)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        let response = &accepted.response;
        let change = accepted.changed.then(|| response.describe());
        let appended = author
            .record_on(
                &mut tx,
                response.status(),
                change.as_deref(),
                response.target(),
            )
            .await?;
        Self::commit_environment_changes(
            tx,
            accepted.environments.iter().map(EnvironmentId::as_str),
        )
        .await?;
        appended.committed();
        Ok((accepted.response, accepted.wake))
    }

    /// Checks that `actor` may submit `mutation` (see `check_request_on`), and
    /// returns the stored response when `request_id` replays the same
    /// request, so callers can return it before checking
    /// preconditions that may have changed since it was accepted, and refuse
    /// a caller before doing any work for it.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn replay(
        &self,
        actor: Actor<'_>,
        mutation: &Mutation,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<Option<MutationResponse>, StoreError> {
        let mut tx = self.snapshot().await?;
        let fingerprint = Self::mutation_fingerprint(mutation, force)?;
        let checked =
            Self::check_request_on(&mut tx, actor, mutation, &fingerprint, request_id, now_ms())
                .await?;
        Ok(match checked {
            Checked::Replay(response) => Some(response),
            Checked::New { .. } => None,
        })
    }

    /// Loads `actor`'s authority and checks that it may submit `mutation`
    /// against the current application, before any replay, then returns the
    /// stored response when `request_id` replays `fingerprint` (see
    /// `replay_on`). Replaying a save may also pass the check a creation
    /// passes, since it may have created that application.
    async fn check_request_on(
        tx: &mut Transaction<'_, Sqlite>,
        actor: Actor<'_>,
        mutation: &Mutation,
        fingerprint: &str,
        request_id: Option<&str>,
        now: i64,
    ) -> Result<Checked, StoreError> {
        let caller = actor.load(tx).await?;
        let owner = caller.as_ref().map(|authority| authority.user_id.as_str());
        let replay = Self::replay_on(tx, request_id, owner, fingerprint, now).await;
        let replaying = matches!(replay, Ok(Some(_)));
        let creating = match &caller {
            Some(authority) => {
                Self::authorize_mutation_on(tx, &authority.grants, mutation, replaying).await?
            }
            None => false,
        };
        Ok(match replay? {
            Some(response) => Checked::Replay(response),
            None => Checked::New { caller, creating },
        })
    }

    /// Records who requested a mutation (an account and its credential, or
    /// the host operator) on the events it wrote, those since `first_event`
    /// in this transaction, and on the operations it created or restarted,
    /// whose later events inherit it. Those are left `requested` with new
    /// events; operations it superseded or returned unchanged, e.g. by
    /// repeating a request, keep their actor.
    async fn attribute_on(
        tx: &mut Transaction<'_, Sqlite>,
        by: super::Attribution<'_>,
        first_event: i64,
    ) -> Result<(), StoreError> {
        let operator = by.operator_uid();
        let system = by.system.map(piqueld_core::sync::SystemActor::as_str);
        sqlx::query!(
            "UPDATE operations SET actor_user_id=?1,actor_credential_id=?2,actor_operator_uid=?3,actor_system=?5
             WHERE state='requested'
             AND id IN (SELECT operation_id FROM events WHERE id>=?4 AND operation_id IS NOT NULL)",
            by.user_id,
            by.credential_id,
            operator,
            first_event,
            system
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE events SET actor_user_id=?1,actor_credential_id=?2,actor_operator_uid=?3,actor_system=?5 WHERE id>=?4",
            by.user_id,
            by.credential_id,
            operator,
            first_event,
            system
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Checks that a caller holding `grants` may submit `mutation` against the
    /// current application and its repository connection (see
    /// `Grants::require_change` and `Mutation::required`), and returns whether
    /// it creates one. Changes to an environment are checked on its application,
    /// with previews permissions also applying to a preview; an unknown
    /// environment is hidden unless the caller could act on any.
    /// When `replaying`, a save naming an existing application also passes with
    /// the permissions that would create it, as that may be what it did.
    async fn authorize_mutation_on(
        tx: &mut Transaction<'_, Sqlite>,
        grants: &Grants,
        mutation: &Mutation,
        replaying: bool,
    ) -> Result<bool, StoreError> {
        let whole = |application| EnvironmentOwner {
            application,
            preview: false,
        };
        let owner = match mutation {
            Mutation::Save { application, .. } => {
                Self::application_id_by_name_on(tx, application.metadata().name.as_str())
                    .await?
                    .map(whole)
            }
            Mutation::Edit { id, .. }
            | Mutation::DeleteApplication { id, .. }
            | Mutation::CreateEnvironment {
                application: id, ..
            }
            | Mutation::Preview(
                PreviewMutation::Create {
                    application: id, ..
                }
                | PreviewMutation::Local {
                    application: id, ..
                },
            ) => Some(whole(id.clone())),
            Mutation::RenameEnvironment { id, .. }
            | Mutation::SetBranch { id, .. }
            | Mutation::SetSync { id, .. }
            | Mutation::SetTelemetry { id, .. }
            | Mutation::Sync(crate::ListedHead { id, .. })
            | Mutation::Promotion(
                PromotionMutation::SetSource { id, .. } | PromotionMutation::Promote { id, .. },
            )
            | Mutation::Deploy { id, .. }
            | Mutation::Delete { id }
            | Mutation::Reconcile { id }
            | Mutation::Stop { id }
            | Mutation::Preview(
                PreviewMutation::Deploy { id }
                | PreviewMutation::Delete { id }
                | PreviewMutation::Prune { id, .. },
            ) => Self::environment_owner_on(tx, id).await?,
            Mutation::Restore(restore) => Self::environment_owner_on(tx, restore.parts().0).await?,
        };
        let target = match (&owner, mutation) {
            (None, Mutation::Save { .. }) => Target::New,
            (Some(owner), Mutation::Save { .. }) => Target::Named(&owner.application),
            (owner, _) => EnvironmentOwner::target(owner.as_ref()),
        };
        let connection = match &owner {
            Some(owner) => Self::application_on(tx, owner.application.as_str())
                .await?
                .and_then(|current| current.application.spec().manifest.clone()),
            None => None,
        };
        if let Some(permission) = mutation.required_installation() {
            grants
                .require(piqueld_core::access::Permission::Global(permission))
                .map_err(StoreError::Denied)?;
        }
        let required = mutation.required(connection.as_ref());
        match grants.require_change(required, target) {
            Err(_) if replaying && matches!(target, Target::Named(_)) => {
                grants.require_change(required, Target::New)
            }
            checked => checked,
        }
        .map_err(StoreError::Denied)?;
        Ok(matches!(target, Target::New))
    }

    /// Hashes the request so a reused request ID can be told apart from a
    /// rerun of the same command. It covers the mutation and `force`, but
    /// not the preconditions the caller inspected beforehand, the expected
    /// generation and application ID: a rerun inspects them again after the
    /// first run changed them, and must still replay.
    fn mutation_fingerprint(mutation: &Mutation, force: bool) -> Result<String, StoreError> {
        match mutation {
            Mutation::Save {
                application,
                expected_application_id: Some(_),
                deploy,
            } => Self::fingerprint(
                &SaveIdentity {
                    application: application.as_ref(),
                    expected_application_id: None,
                    deploy: *deploy,
                },
                force,
            ),
            mutation => Self::fingerprint(mutation, force),
        }
    }

    /// Hashes `request` and `force`. `null` stands where the expected
    /// generation once was, so receipts of requests sent without one keep
    /// their fingerprint.
    fn fingerprint(request: &impl serde::Serialize, force: bool) -> Result<String, StoreError> {
        Ok(format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(request, None::<u64>, force)).map_err(StoreError::corrupt)?
            )
        ))
    }

    /// Returns the stored response for an unexpired receipt of `owner` (the
    /// account, or `None` for the daemon and the host operator, which may act
    /// on everything) with the request's `fingerprint`, `ReplayConflict` for
    /// another owner's or a mismatched one, or `None` without a receipt.
    async fn replay_on(
        connection: &mut SqliteConnection,
        request_id: Option<&str>,
        owner: Option<&str>,
        fingerprint: &str,
        now: i64,
    ) -> Result<Option<MutationResponse>, StoreError> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        let receipt = sqlx::query!("SELECT fingerprint,response_json,user_id FROM request_receipts WHERE request_id=?1 AND expires_at_ms>?2",request_id,now)
            .fetch_optional(connection).await.map_err(StoreError::database)?;
        receipt
            .map(|receipt| {
                if receipt.user_id.as_deref() != owner || receipt.fingerprint != fingerprint {
                    return Err(StoreError::ReplayConflict);
                }
                serde_json::from_str(&receipt.response_json).map_err(StoreError::corrupt)
            })
            .transpose()
    }

    /// Loads the mutation's application or environment, checks the revision
    /// precondition against its application (an absent one counts as revision
    /// zero), then dispatches to the transactional handler.
    async fn execute_mutation(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        mutation: Mutation,
        expected: Option<u64>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        match mutation {
            Mutation::Save {
                application,
                expected_application_id,
                deploy,
            } => {
                let current =
                    Self::find_by_name_on(tx, application.metadata().name.as_str()).await?;
                Self::check_save(current.as_ref(), expected)?;
                let application = application.with_id(Self::application_identity(
                    current.as_ref(),
                    expected_application_id.as_ref(),
                )?);
                let mut saved = Self::save_configuration_on(tx, &application, expected).await?;
                Self::deploy_saved(tx, application.id(), &mut saved, deploy).await?;
                Ok(Accepted {
                    changed: true,
                    environments: Self::environment_ids_on(tx, application.id()).await?,
                    response: MutationResponse::Saved(saved),
                    wake: deploy,
                })
            }
            Mutation::Edit { id, edit, deploy } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                Box::pin(Self::accept_edit(tx, current, *edit, deploy, now)).await
            }
            Mutation::DeleteApplication { id, environments } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                Self::delete_application_on(tx, current, environments, now).await
            }
            Mutation::CreateEnvironment {
                application,
                name,
                source,
            } => {
                let current = Self::checked_application_on(tx, &application, expected).await?;
                let id = Self::create_environment_on(tx, &current, &name, source, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::RenameEnvironment { id, name } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let changed = Self::rename_environment_on(tx, &current, &name, now).await?;
                Ok(Self::environment_accepted(tx, id).await?.changing(changed))
            }
            Mutation::SetBranch { id, branch } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let changed = Self::set_branch_on(tx, &current, branch, now).await?;
                Ok(Self::environment_accepted(tx, id).await?.changing(changed))
            }
            Mutation::Deploy {
                id,
                revision,
                rebuild,
            } => {
                Box::pin(Self::deploy_on(
                    tx,
                    &id,
                    revision.as_ref(),
                    rebuild,
                    expected,
                ))
                .await
            }
            Mutation::SetSync { id, enabled } => Self::set_sync_on(tx, id, enabled, now).await,
            Mutation::SetTelemetry { id, granted } => {
                Self::set_telemetry_on(tx, id, granted, now).await
            }
            Mutation::Sync(listed) => Self::sync_on(tx, listed, now).await,
            Mutation::Delete { id } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let (operation, requested) =
                    match Self::pending_deletion_on(tx, &current.environment).await? {
                        Some(operation) => (operation, false),
                        None => (Self::delete_environment_on(tx, &current).await?, true),
                    };
                Ok(Self::operation_accepted(&operation).changing(requested))
            }
            Mutation::Reconcile { id } => Self::start_on(tx, &id, expected, now).await,
            Mutation::Stop { id } => Self::stop_on(tx, &id, expected, now).await,
            Mutation::Restore(restore) => Box::pin(Self::restore_on(tx, restore)).await,
            Mutation::Promotion(mutation) => self.promotion_on(tx, mutation, expected, now).await,
            Mutation::Preview(mutation) => {
                Box::pin(self.execute_preview_on(tx, mutation, now)).await
            }
        }
    }

    /// Deploys environment `id` from its source, at `revision` when given,
    /// after checking the revision precondition against its application.
    /// With `rebuild`, the deployment builds every Git source, even one whose
    /// image it could reuse.
    async fn deploy_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        revision: Option<&piqueld_core::manifest::ManifestRevision>,
        rebuild: bool,
        expected: Option<u64>,
    ) -> Result<Accepted, StoreError> {
        let current = Self::checked_environment_on(tx, id, expected).await?;
        let operation = Self::deploy_revision_on(tx, &current, revision).await?;
        if rebuild {
            Self::request_rebuild_on(tx, &operation.id).await?;
        }
        Ok(Self::operation_accepted(&operation))
    }

    /// Changes where an environment's releases come from, or promotes a
    /// release into a promoted environment, after checking the revision
    /// precondition against its application.
    async fn promotion_on(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        mutation: PromotionMutation,
        expected: Option<u64>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        match mutation {
            PromotionMutation::SetSource { id, promote_from } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let changed = Self::set_promotion_on(tx, &current, promote_from, now).await?;
                Ok(Self::environment_accepted(tx, id).await?.changing(changed))
            }
            PromotionMutation::Promote { id, promotion, .. } => {
                let promotion = promotion.ok_or(StoreError::InvalidInput)?;
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let instance = self.instance_id().clone();
                let operation =
                    Box::pin(self.accept_promotion_on(tx, &current, &promotion, instance)).await?;
                Ok(Accepted {
                    changed: true,
                    response: MutationResponse::Promotion(piqueld_core::api::AcceptedPromotion {
                        operation: AcceptedOperation::from(&operation),
                        release_id: promotion.release,
                        origin: promotion.origin,
                    }),
                    wake: true,
                    environments: vec![id],
                })
            }
        }
    }

    /// Creates, deploys, or deletes a preview. Previews are found by ID only
    /// among previews, and never check or advance the application revision.
    async fn execute_preview_on(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        mutation: PreviewMutation,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        match mutation {
            PreviewMutation::Create {
                application,
                branch,
                slot,
            } => {
                let (preview, created) = self
                    .find_preview_on(tx, &application, &branch, slot.as_ref(), now)
                    .await?;
                let (operation, deployed) =
                    match Self::latest_operation_on(tx, preview.id()).await? {
                        Some(operation) if !created => (operation, false),
                        _ => (Self::deploy_preview_on(tx, &preview).await?, true),
                    };
                let accepted = Self::preview_accepted(preview, &operation, created, created);
                Ok(accepted.changing(created || deployed))
            }
            PreviewMutation::Local {
                application,
                branch,
                slot,
                repository,
                snapshot,
            } => {
                let (preview, created) = self
                    .find_preview_on(tx, &application, &branch, slot.as_ref(), now)
                    .await?;
                if preview
                    .tracking()?
                    .repository()
                    .is_some_and(|current| current.repository.url != repository)
                {
                    return Err(StoreError::IdentityConflict);
                }
                let operation = Self::deploy_snapshot_on(tx, &preview, &snapshot).await?;
                Ok(Self::preview_accepted(preview, &operation, created, true))
            }
            PreviewMutation::Deploy { id } => {
                let preview = Self::preview_on(tx, &id).await?;
                let operation = Self::deploy_preview_on(tx, &preview).await?;
                Ok(Self::operation_accepted(&operation))
            }
            PreviewMutation::Delete { id } => {
                let preview = Self::preview_on(tx, &id).await?;
                Self::delete_preview_on(tx, &preview).await
            }
            PreviewMutation::Prune { id, repository } => {
                let preview = Self::preview_on(tx, &id).await?;
                if preview
                    .tracking()?
                    .repository()
                    .is_none_or(|current| current.repository.url != repository)
                    || Self::preview_origins_on(tx, &id).await?.local()
                {
                    return Err(StoreError::IdentityConflict);
                }
                Self::delete_preview_on(tx, &preview).await
            }
        }
    }

    /// The preview of `branch` and `slot` of `application`, created first
    /// if needed (see `create_preview_on`), and whether it was.
    async fn find_preview_on(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        branch: &piqueld_core::GitBranch,
        slot: Option<&piqueld_core::PreviewSlot>,
        now: i64,
    ) -> Result<(super::StoredEnvironment, bool), StoreError> {
        let current = Self::checked_application_on(tx, application, None).await?;
        let (id, created) =
            Self::create_preview_on(tx, &current, branch, slot, &self.previews, now).await?;
        Ok((Self::preview_on(tx, &id).await?, created))
    }

    /// Responds with `preview`, whether this request `created` it, and its
    /// `operation`, waking the controller when it was `deployed` now.
    fn preview_accepted(
        preview: super::StoredEnvironment,
        operation: &Operation,
        created: bool,
        deployed: bool,
    ) -> Accepted {
        let id = preview.id().clone();
        Accepted {
            changed: true,
            wake: deployed,
            response: MutationResponse::Preview(Box::new(CreatedPreview {
                preview: preview.environment,
                operation: AcceptedOperation::from(operation),
                created,
            })),
            environments: vec![id],
        }
    }

    /// Requests a preview's deletion, or returns the one already pending.
    async fn delete_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        preview: &super::StoredEnvironment,
    ) -> Result<Accepted, StoreError> {
        let (operation, requested) = Self::deletion_on(tx, &preview.environment).await?;
        Ok(Self::operation_accepted(&operation).changing(requested))
    }

    /// Requests `environment`'s deletion (see `request_delete_on`), or
    /// returns the one already pending, and whether it requested one.
    async fn deletion_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &EnvironmentView,
    ) -> Result<(Operation, bool), StoreError> {
        match Self::pending_deletion_on(tx, environment).await? {
            Some(operation) => Ok((operation, false)),
            None => Ok((Self::request_delete_on(tx, &environment.id).await?, true)),
        }
    }

    /// The deletion already requested for `environment`, which a repeated
    /// request returns rather than requesting another.
    async fn pending_deletion_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &EnvironmentView,
    ) -> Result<Option<Operation>, StoreError> {
        Ok(Self::latest_operation_on(tx, &environment.id)
            .await?
            .filter(|operation| {
                environment.delete_intent && operation.kind == OperationKind::Delete
            }))
    }

    /// Loads an application after checking the revision precondition, so a
    /// stale revision is reported as a conflict before absence.
    async fn checked_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<StoredApplication, StoreError> {
        let current = Self::application_on(tx, id.as_str()).await?;
        Self::check_generation(expected, current.as_ref().map_or(0, |app| app.generation))?;
        current.ok_or(StoreError::NotFound)
    }

    /// Loads an environment, then checks the revision precondition against
    /// its application's revision. Previews are `NotFound`: environment
    /// changes never act on them. Absence wins over a stale revision.
    pub(super) async fn checked_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        expected: Option<u64>,
    ) -> Result<super::StoredEnvironment, StoreError> {
        let current = Self::environment_on(tx, id.as_str())
            .await?
            .filter(|current| current.environment.preview().is_none())
            .ok_or(StoreError::NotFound)?;
        Self::check_generation(expected, current.application.generation)?;
        Ok(current)
    }

    /// Advances the application revision past `current` after an environment
    /// is created, renamed, or deleted, so a caller that inspected the
    /// application before the change fails its precondition. Environments
    /// whose resolved target was current stay current, except `stale`: a
    /// renamed environment whose configuration renders differently.
    pub(super) async fn bump_generation_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        current: u64,
        stale: Option<&EnvironmentId>,
    ) -> Result<(), StoreError> {
        let previous = i64::try_from(current).map_err(StoreError::invalid_input)?;
        let generation = previous.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let id = application.as_str();
        sqlx::query!(
            "UPDATE applications SET generation=?1 WHERE id=?2",
            generation,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let stale = stale.map(EnvironmentId::as_str);
        sqlx::query!(
            "UPDATE environments SET resolved_generation=?1 WHERE application_id=?2 AND resolved_generation=?3 AND id IS NOT ?4",
            generation,
            id,
            previous,
            stale
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Responds with an accepted environment operation.
    pub(super) fn operation_accepted(operation: &Operation) -> Accepted {
        Accepted {
            changed: true,
            response: MutationResponse::Operation(AcceptedOperation::from(operation)),
            wake: true,
            environments: vec![operation.environment_id.clone()],
        }
    }

    /// Responds with the current state of a created or renamed environment.
    pub(super) async fn environment_accepted(
        tx: &mut Transaction<'_, Sqlite>,
        id: EnvironmentId,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::existing_environment_on(tx, id.as_str()).await?;
        Ok(Accepted {
            changed: true,
            response: MutationResponse::Environment(environment.environment),
            wake: false,
            environments: vec![id],
        })
    }

    /// The IDs of an application's environments.
    async fn environment_ids_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
    ) -> Result<Vec<EnvironmentId>, StoreError> {
        Ok(Self::environments_on(tx, application.as_str())
            .await?
            .into_iter()
            .map(|environment| environment.id)
            .collect())
    }

    /// Fetches an environment's newest operation inside `tx`.
    pub(super) async fn latest_operation_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
    ) -> Result<Option<Operation>, StoreError> {
        let id = id.as_str();
        let latest = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,id)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
        match latest {
            Some(id) => Ok(Some(Self::operation_on(tx, &id).await?)),
            None => Ok(None),
        }
    }

    /// Applies a single field edit to the saved manifest, revalidates it, and
    /// saves it (renames go through `rename_on`), optionally starting a deployment.
    /// Edits of a repository-backed application apply to the manifests its
    /// deployments fetch (see `piqueld_core::manifest::LocalEdits`);
    /// applications being deleted are `Busy`. Records an `application_edited` event naming the
    /// edited field and resource once, in the application's history.
    async fn accept_edit(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        edit: piqueld_core::edit::ApplicationEdit,
        deploy: bool,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        use piqueld_core::edit::ApplicationEdit;
        if current.delete_intent {
            return Err(StoreError::Busy);
        }
        let id = current.application.id().clone();
        let mut manifest = current.application.to_manifest();
        edit.clone().apply(&mut manifest)?;
        Self::declare_edited_on(
            tx,
            &id,
            current.application.spec(),
            &mut manifest.spec,
            edit.environment(),
        )
        .await?;
        let application = manifest.validate_template()?.normalize(id.clone());
        let resource = match &edit {
            ApplicationEdit::Service { name, .. } | ApplicationEdit::RemoveService(name) => {
                Some(name.as_str())
            }
            ApplicationEdit::AddService(service) => Some(service.name.as_str()),
            _ => None,
        };
        let (mut saved, changed) = if let ApplicationEdit::Name(name) = edit {
            Self::rename_on(tx, current, name, now).await?
        } else {
            let saved =
                Self::save_configuration_on(tx, &application, Some(current.generation)).await?;
            let generation = i64::try_from(saved.generation).map_err(StoreError::corrupt)?;
            let app_id = id.as_str();
            let detail = serde_json::to_string(&EventDetail::Edited {
                field: edit.field(),
            })
            .map_err(StoreError::corrupt)?;
            let event_kind = EventKind::ApplicationEdited.as_str();
            sqlx::query!("INSERT INTO events(application_id,generation,kind,message,detail_json,resource,created_at_ms) VALUES(?1,?2,?6,'Saved application configuration',?3,?4,?5)",app_id,generation,detail,resource,now,event_kind).execute(&mut **tx).await.map_err(StoreError::database)?;
            (saved, true)
        };
        Self::deploy_saved(tx, &id, &mut saved, deploy).await?;
        Ok(Accepted {
            changed: changed || deploy,
            environments: Self::environment_ids_on(tx, &id).await?,
            response: MutationResponse::Saved(saved),
            wake: deploy,
        })
    }

    /// When `deploy` is set, starts a deployment of the just-saved
    /// configuration, from its branch when repository-backed, to the
    /// application's only environment or, when its manifest declares
    /// environments, to every declared one that builds: promoted ones wait
    /// for a promotion. Records the operation IDs on `saved`.
    async fn deploy_saved(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        saved: &mut SavedApplication,
        deploy: bool,
    ) -> Result<(), StoreError> {
        if !deploy {
            return Ok(());
        }
        let application = Self::application_on(tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let declared = &application.application.spec().environments;
        let targets = if declared.is_empty() {
            vec![Self::sole_environment_on(tx, id).await?]
        } else {
            Self::environments_on(tx, id.as_str())
                .await?
                .into_iter()
                .filter(|environment| {
                    declared
                        .get(environment.name.as_str())
                        .is_some_and(|config| config.promote_from.is_none())
                        && !environment.delete_intent
                        && environment.source.promoted_from().is_none()
                })
                .map(|environment| environment.id)
                .collect()
        };
        for environment in targets {
            let environment = Self::existing_environment_on(tx, environment.as_str()).await?;
            let operation = Self::deploy_revision_on(tx, &environment, None).await?;
            saved.operation_ids.push(operation.id);
        }
        saved.operation_id = saved.operation_ids.first().cloned();
        Ok(())
    }

    /// Checks saving over `current`, the application that has the saved
    /// name, if any, against the `expected` generation. Generation zero
    /// creates, so a taken name is `AlreadyExists`, as for renames. Otherwise
    /// a stale revision is a generation conflict.
    fn check_save(
        current: Option<&StoredApplication>,
        expected: Option<u64>,
    ) -> Result<(), StoreError> {
        if expected == Some(0) && current.is_some() {
            return Err(StoreError::AlreadyExists);
        }
        Self::check_generation(expected, current.map_or(0, |app| app.generation))
    }

    /// Resolves the ID for a save by name: the existing application's ID,
    /// or a freshly generated one. Fails with `IdentityConflict` when the caller
    /// expected an ID that the name no longer selects.
    fn application_identity(
        current: Option<&StoredApplication>,
        expected: Option<&ApplicationId>,
    ) -> Result<ApplicationId, StoreError> {
        if expected.is_some_and(|id| current.is_none_or(|app| app.application.id() != id)) {
            return Err(StoreError::IdentityConflict);
        }
        current.map_or_else(
            || ApplicationId::parse(super::new_id("app")).map_err(StoreError::corrupt),
            |app| Ok(app.application.id().clone()),
        )
    }

    /// Renames an idle application that is not repository-backed: its name
    /// must match the manifests it fetches. A real name
    /// change bumps the generation (and each environment's resolved generation
    /// when it was current and its configuration does not render the name,
    /// e.g. `${{ app.name }}`), updates the display name of every environment's
    /// resolved and latest targets, and records an `application_renamed` event
    /// once, in the application's history. Its current name changes nothing.
    /// Returns the application's revision, and whether it renamed. Never
    /// starts a deployment.
    async fn rename_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        name: String,
        now: i64,
    ) -> Result<(SavedApplication, bool), StoreError> {
        if current.application.spec().manifest.is_some() {
            return Err(StoreError::RepositoryManaged);
        }
        let id = current.application.id().as_str().to_owned();
        let busy = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM environments e WHERE e.application_id=?1 AND (e.delete_intent=1 OR (SELECT state FROM operations o WHERE o.environment_id=e.id ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) IN ('requested','running')))",id)
            .fetch_one(&mut **tx).await.map_err(StoreError::database)?;
        if current.delete_intent || busy != 0 {
            return Err(StoreError::Busy);
        }
        let saved = |generation| SavedApplication {
            application_id: current.application.id().clone(),
            generation,
            operation_id: None,
            operation_ids: Vec::new(),
        };
        let old_name = current.application.metadata().name.clone();
        if old_name.as_str() == name {
            return Ok((saved(current.generation), false));
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::InvalidInput)?;
        let previous = i64::try_from(current.generation).map_err(StoreError::invalid_input)?;
        let revision = i64::try_from(generation).map_err(StoreError::invalid_input)?;
        let application = current.application.clone().with_name(
            piqueld_core::ApplicationName::parse(name.clone())
                .map_err(StoreError::invalid_input)?,
        );
        let desired = serde_json::to_string(&application).map_err(StoreError::corrupt)?;
        sqlx::query!("UPDATE applications SET name=?1,desired_json=?2,generation=?3,updated_at_ms=?4 WHERE id=?5",name,desired,revision,now,id)
            .execute(&mut **tx).await.map_err(StoreError::constraint)?;
        sqlx::query!("UPDATE environments SET resolved_json=CASE WHEN resolved_json IS NULL THEN NULL ELSE json_set(resolved_json,'$.name',?1) END WHERE application_id=?2",name,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        for environment in Self::deployables_on(tx, &id).await? {
            let target = environment.target();
            if current
                .application
                .renders_like(&target, &application, &target)
            {
                let environment = environment.id.as_str();
                sqlx::query!("UPDATE environments SET resolved_generation=?1 WHERE id=?2 AND resolved_generation=?3",revision,environment,previous)
                    .execute(&mut **tx).await.map_err(StoreError::database)?;
            }
        }
        sqlx::query!("UPDATE operations SET target_json=json_set(target_json,'$.name',?1) WHERE target_json IS NOT NULL AND id IN (SELECT (SELECT o.id FROM operations o WHERE o.environment_id=e.id ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) FROM environments e WHERE e.application_id=?2)",name,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        let message = format!("renamed {old_name} to {name}");
        let event_kind = EventKind::ApplicationRenamed.as_str();
        sqlx::query!("INSERT INTO events(application_id,generation,kind,message,created_at_ms) VALUES(?1,?2,?5,?3,?4)",id,revision,message,now,event_kind).execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok((saved(generation), true))
    }

    /// Requests deletion of an application, every environment and every
    /// preview. With several environments, `confirmed` must name each of them;
    /// previews need no confirmation. Those already being deleted keep their
    /// pending operation. An application without any is removed immediately.
    async fn delete_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        mut confirmed: Vec<EnvironmentName>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let id = current.application.id().as_str().to_owned();
        let environments = Self::deployables_on(tx, &id).await?;
        let names = environments
            .iter()
            .filter(|environment| environment.preview().is_none())
            .map(|environment| environment.name.clone())
            .collect::<Vec<_>>();
        confirmed.sort();
        confirmed.dedup();
        if names.len() > 1 && confirmed != names {
            return Err(StoreError::ConfirmationRequired {
                environments: names,
            });
        }
        let mut generation = current.generation;
        if !current.delete_intent {
            generation = generation.checked_add(1).ok_or(StoreError::InvalidInput)?;
            let revision = i64::try_from(generation).map_err(StoreError::invalid_input)?;
            sqlx::query!(
                "UPDATE applications SET delete_intent=1,generation=?1,updated_at_ms=?2 WHERE id=?3",
                revision,
                now,
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        let mut changed = !current.delete_intent || environments.is_empty();
        let mut operations = Vec::with_capacity(environments.len());
        for environment in &environments {
            let (operation, requested) = Self::deletion_on(tx, environment).await?;
            changed |= requested;
            operations.push(AcceptedOperation::from(&operation));
        }
        Self::finish_application_delete_on(tx, &id).await?;
        Ok(Accepted {
            changed,
            wake: !operations.is_empty(),
            response: MutationResponse::Deleted(DeletedApplication {
                application_id: current.application.id().clone(),
                generation,
                operations,
            }),
            environments: environments
                .into_iter()
                .map(|environment| environment.id)
                .collect(),
        })
    }

    /// Deletes expired idempotency receipts.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_receipts(&self) -> Result<(), StoreError> {
        let now = now_ms();
        self.write(sqlx::query!(
            "DELETE FROM request_receipts WHERE expires_at_ms<=?1",
            now
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
