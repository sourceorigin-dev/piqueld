//! Durable candidate manifests for manual deployments.
//!
//! `deployment_inputs` starts with the configuration captured by Deploy, then
//! holds the validated manifest and repository commit once fetched, which
//! retries reuse. The candidate becomes its environment's last fetched
//! manifest only after source preparation succeeds. The input also captures
//! the manifest the application last read from its repository and records
//! the one it fetched.
use super::{Operation, Store, StoreError, now_ms};
use piqueld_core::EventKind;
use piqueld_core::{
    EnvironmentKind,
    api::{DeploymentOrigin, DiagnosticView},
    manifest::{ApplicationTemplate, Rendering, RepositoryManifest},
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// A deployment's candidate manifest and whether it is the fetched snapshot.
pub struct DeploymentInput {
    /// The candidate manifest, with references unresolved.
    pub template: ApplicationTemplate,
    /// Whether it was fetched from the repository rather than saved.
    pub fetched: bool,
    /// Commit the fetched manifest was read from.
    pub commit: Option<String>,
    /// The branch its environment tracked, unpinned, when it was requested.
    pub tracked_branch: Option<String>,
    /// Where the deployment comes from: a local snapshot is fetched from
    /// its own repository instead of the branch.
    pub origin: DeploymentOrigin,
    /// The manifest the application had last read from its repository when
    /// the deployment was requested.
    repository: Option<ApplicationTemplate>,
}

impl DeploymentInput {
    /// Whether `fetched` is still the manifest the application had last read
    /// from its repository, so a push since left the manifest unchanged.
    #[must_use]
    pub fn unchanged(&self, fetched: &ApplicationTemplate) -> bool {
        self.repository
            .as_ref()
            .is_some_and(|repository| repository.spec_hash() == fetched.spec_hash())
    }
}

impl Store {
    /// Captures the configuration a Deploy started from, still unfetched, with
    /// the branch its environment tracks unpinned now and the application's
    /// repository manifest.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn insert_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        application: &ApplicationTemplate,
    ) -> Result<(), StoreError> {
        let json = application.canonical_json().map_err(StoreError::corrupt)?;
        let environment = operation.environment_id.as_str();
        sqlx::query!(
            "INSERT INTO deployment_inputs(operation_id,application_json,tracked_branch,repository_json) VALUES(?1,?2,(SELECT branch FROM environments WHERE id=?3 AND pinned_commit IS NULL),(SELECT a.repository_json FROM applications a JOIN environments e ON e.application_id=a.id WHERE e.id=?3))",
            operation.id,
            json,
            environment
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Loads the operation's candidate manifest, if one was captured.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn deployment_input(
        &self,
        operation: &Operation,
    ) -> Result<Option<DeploymentInput>, StoreError> {
        sqlx::query!(
            r#"SELECT i.application_json,i.fetched,i.repository_commit,i.tracked_branch,i.repository_json,COALESCE(d.origin_json,'{"type":"build"}') AS "origin_json!: String" FROM deployment_inputs i LEFT JOIN deployments d ON d.id=i.operation_id WHERE i.operation_id=?1"#,
            operation.id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(DeploymentInput {
                template: serde_json::from_str(&row.application_json)
                    .map_err(StoreError::corrupt)?,
                fetched: row.fetched != 0,
                commit: row.repository_commit,
                tracked_branch: row.tracked_branch,
                origin: serde_json::from_str(&row.origin_json).map_err(StoreError::corrupt)?,
                repository: row
                    .repository_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(StoreError::corrupt)?,
            })
        })
        .transpose()
    }

    /// Stores the fetched manifest and commit for the latest running operation,
    /// with `fetched`, the manifest as read from the repository, which the saved
    /// one may replace. Copies the manifest, its rendering and `warnings` into
    /// deployment history, stores generated secret values and pins their versions
    /// (see `pin_secrets_on`), records the `branch_head` sync follows from (see
    /// `record_branch_head_on`) and rechecks hostname reservations. A second save,
    /// or one for superseded work, fails with `StoreError::IllegalTransition`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn save_deployment_input(
        &self,
        operation: &Operation,
        (template, fetched): (&ApplicationTemplate, Option<&ApplicationTemplate>),
        rendering: &Rendering,
        (commit, branch_head): (Option<&str>, Option<&super::BranchHead>),
        warnings: &[DiagnosticView],
    ) -> Result<(), StoreError> {
        let json = template.canonical_json().map_err(StoreError::corrupt)?;
        let fetched = fetched
            .map(|fetched| fetched.clone().without_files().canonical_json())
            .transpose()
            .map_err(StoreError::corrupt)?;
        let (manifest, variables) = Self::snapshot_json(Some(rendering))?;
        let warnings = serde_json::to_string(warnings).map_err(StoreError::corrupt)?;
        let application = &rendering.application;
        let generated = self
            .generated_secrets(&operation.environment_id, application)
            .await?;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let app_id = operation.environment_id.as_str();
        let changed = sqlx::query!("UPDATE deployment_inputs SET application_json=?1,repository_commit=?2,fetched_json=?5,fetched=1 WHERE operation_id=?3 AND fetched=0 AND operation_id=(SELECT id FROM operations WHERE environment_id=?4 ORDER BY created_at_ms DESC,id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?3 AND state='running')", json,commit,operation.id,app_id,fetched).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!(
            "UPDATE deployments SET manifest_json=?1,template_json=?2,variables_json=?3,warnings_json=?4 WHERE id=?5",
            manifest,
            json,
            variables,
            warnings,
            operation.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let now = now_ms();
        Self::operation_event(
            &mut tx,
            &operation.id,
            EventKind::ManifestFetched,
            commit,
            now,
        )
        .await?;
        if let Some(head) = branch_head {
            Self::record_branch_head_on(&mut tx, &operation.environment_id, head, now).await?;
        }
        self.pin_secrets_on(&mut tx, operation, application, &generated)
            .await?;
        Self::commit_environment_changes(tx, [app_id]).await
    }

    /// Records a fetched repository manifest, under the application's current
    /// name, as its environment's last fetched one, while the environment still
    /// follows a branch; others keep their own. While the application is
    /// repository-backed, a deployment from the path, branch and commit
    /// `spec.manifest` names (see `reads_connection_on`) also converges the
    /// declared environments and saves a manifest a push changed (see
    /// `read_repository_on`). Records `application_applied` when any manifest
    /// changed. Fails with `secret_name_conflict` when the application's store
    /// holds a name the manifest declares. Called in the transaction that saves
    /// the prepared runtime target.
    pub(super) async fn accept_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        fetching: &EnvironmentKind,
    ) -> Result<(), StoreError> {
        let row = sqlx::query!(
            "SELECT application_json,repository_json,fetched_json FROM deployment_inputs WHERE operation_id=?1 AND fetched=1 AND repository_commit IS NOT NULL",
            operation.id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(());
        };
        let deployed: ApplicationTemplate =
            serde_json::from_str(&row.application_json).map_err(StoreError::corrupt)?;
        let stored = Self::application_on(tx, deployed.id().as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let application = &stored.application;
        Self::check_declared_against_store_on(
            tx,
            application.id().as_str(),
            &deployed.spec().secrets,
        )
        .await?;
        let deployed = deployed
            .with_name(application.metadata().name.clone())
            .without_files();
        let manifest = deployed.canonical_json().map_err(StoreError::corrupt)?;
        let now = now_ms();
        let environment = operation.environment_id.as_str();
        let mut changed = sqlx::query!(
            "UPDATE environments SET manifest_json=?1,updated_at_ms=?2 WHERE id=?3 AND branch IS NOT NULL AND manifest_json IS NOT ?1",
            manifest,
            now,
            environment
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if let Some(connection) =
            Self::reads_connection_on(tx, operation, fetching, &deployed, application).await?
        {
            let hash = |json: Option<&str>| {
                json.map(|json| {
                    serde_json::from_str::<ApplicationTemplate>(json)
                        .map(|manifest| manifest.without_files().spec_hash())
                        .map_err(StoreError::corrupt)
                })
                .transpose()
            };
            let read = (
                hash(row.repository_json.as_deref())?,
                hash(row.fetched_json.as_deref())?,
            );
            changed += Self::read_repository_on(
                tx,
                operation,
                &stored,
                (deployed.with_manifest(Some(connection)), read),
                now,
            )
            .await?;
        }
        if changed > 0 {
            Self::operation_event(tx, &operation.id, EventKind::ApplicationApplied, None, now)
                .await?;
        }
        Ok(())
    }

    /// Converges the environments of `stored` to the ones its saved manifest
    /// declares, retrying changes that waited, after saving `deployed`, which
    /// `operation` deployed from its connection, when a push changed the manifest.
    /// `read` holds the spec hashes of the manifest last read when `operation`
    /// was requested and of the one it fetched; a push changed it when they
    /// differ. `deployed` then becomes both the saved and the last-read manifest.
    /// Advances the revision when the saved manifest changes, resolving
    /// `operation` at the new one unless its environment's source changed. Returns
    /// the rows changed.
    async fn read_repository_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        stored: &super::StoredApplication,
        (deployed, (captured, fetched)): (ApplicationTemplate, (Option<String>, Option<String>)),
        now: i64,
    ) -> Result<u64, StoreError> {
        let application = &stored.application;
        let read = Self::repository_manifest_on(tx, stored)
            .await?
            .map(|read| read.spec_hash());
        let pushed = fetched.is_some() && fetched != captured && fetched != read;
        let saved = if pushed { &deployed } else { application };
        let source = async |tx: &mut Transaction<'_, Sqlite>| {
            Self::environment_on(tx, operation.environment_id.as_str())
                .await
                .map(|environment| environment.map(|environment| environment.environment.source))
        };
        let before = source(tx).await?;
        let converged = Self::converge_environments_on(
            tx,
            application.id(),
            &saved
                .spec()
                .declared_environments_since(Some(application.spec()))?,
            Some(&operation.environment_id),
            now,
        )
        .await?;
        let edited = saved != application;
        if converged || edited {
            Self::bump_generation_on(tx, application.id(), stored.generation, None).await?;
        }
        if edited && source(tx).await? == before {
            let (previous, revision) = (
                i64::try_from(stored.generation).map_err(StoreError::corrupt)?,
                i64::try_from(stored.generation + 1).map_err(StoreError::corrupt)?,
            );
            sqlx::query!(
                "UPDATE operations SET generation=?1 WHERE id=?2 AND generation=?3",
                revision,
                operation.id,
                previous
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        if !pushed {
            return Ok(0);
        }
        let deployed = deployed.canonical_json().map_err(StoreError::corrupt)?;
        let id = application.id().as_str();
        Ok(sqlx::query!(
            "UPDATE applications SET desired_json=?1,repository_json=?1,updated_at_ms=?2 WHERE id=?3",
            deployed,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected())
    }

    /// Whether `operation`, a deployment of `kind`, fetched `fetched`
    /// from the connection of `application`, which this returns: the
    /// manifest an environment, not a preview, reads from it.
    /// See [`Self::reads_connection_on`].
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn reads_connection(
        &self,
        operation: &Operation,
        kind: &EnvironmentKind,
        fetched: &ApplicationTemplate,
    ) -> Result<bool, StoreError> {
        let connection = &mut *self.connection().await?;
        let application = Self::application_on(connection, fetched.id().as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok(Self::reads_connection_on(
            connection,
            operation,
            kind,
            fetched,
            &application.application,
        )
        .await?
        .is_some())
    }

    /// See [`Self::reads_connection`].
    async fn reads_connection_on(
        connection: &mut SqliteConnection,
        operation: &Operation,
        kind: &EnvironmentKind,
        fetched: &ApplicationTemplate,
        application: &ApplicationTemplate,
    ) -> Result<Option<RepositoryManifest>, StoreError> {
        let Some(wanted) = application.spec().manifest.clone() else {
            return Ok(None);
        };
        let reads = matches!(kind, EnvironmentKind::Environment)
            && Self::fetched_connection_on(connection, operation, fetched, &wanted).await?;
        Ok(reads.then_some(wanted))
    }

    /// Whether `fetched`, the manifest `operation` fetched, came from the
    /// path and revision `wanted` names. Its own connection is the path
    /// and revision it was fetched from; sync pins the head it listed of a
    /// branch it follows, so its deployments of the connection's unpinned
    /// branch count too.
    async fn fetched_connection_on(
        connection: &mut SqliteConnection,
        operation: &Operation,
        fetched: &ApplicationTemplate,
        wanted: &RepositoryManifest,
    ) -> Result<bool, StoreError> {
        let Some(fetched) = fetched
            .spec()
            .manifest
            .as_ref()
            .filter(|fetched| fetched.path == wanted.path)
            .map(|fetched| &fetched.repository)
        else {
            return Ok(false);
        };
        let wanted = &wanted.repository;
        if fetched == wanted {
            return Ok(true);
        }
        if fetched.url != wanted.url || fetched.branch != wanted.branch || wanted.commit.is_some() {
            return Ok(false);
        }
        Self::requested_by_sync_on(connection, operation).await
    }
}

#[cfg(test)]
mod tests;
