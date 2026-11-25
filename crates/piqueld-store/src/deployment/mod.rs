//! Saved configuration and immutable deployment inputs. Saving never creates work.
use super::{EnvironmentId, NormalizedApplication, Operation, Paging, Store, StoreError, now_ms};
use piqueld_core::{
    api::{
        DeploymentOrigin, DeploymentSummary, DeploymentView, DiagnosticView, Page, SavedApplication,
    },
    manifest::{ApplicationTemplate, Rendering, VariableValue},
};
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;

/// `deployments` row shared by the snapshot and history page queries.
struct DeploymentRow {
    id: String,
    manifest_json: String,
    template_json: String,
    variables_json: String,
    warnings_json: String,
    succeeded_at_ms: Option<i64>,
    release_id: Option<String>,
    origin_json: String,
}

/// Sorts after every deployment ID, so a first page seeks
/// `deployment_environment` as a cursor does.
const NEWEST: &str = "\u{10FFFF}";

/// `deployments` columns of a [`DeploymentSummary`].
struct SummaryRow {
    id: String,
    succeeded_at_ms: Option<i64>,
    release_id: Option<String>,
    origin_json: String,
    git_sha: Option<String>,
    git_branch: Option<String>,
}

impl SummaryRow {
    /// Decodes the row and reads its operation.
    async fn summary_on(
        self,
        tx: &mut Transaction<'_, Sqlite>,
        flags: &Flags,
    ) -> Result<DeploymentSummary, StoreError> {
        Ok(DeploymentSummary {
            operation: Store::operation_on(tx, &self.id).await?,
            origin: serde_json::from_str(&self.origin_json).map_err(StoreError::corrupt)?,
            release: self
                .release_id
                .map(piqueld_core::ReleaseId::parse)
                .transpose()
                .map_err(StoreError::corrupt)?,
            succeeded_at_ms: self.succeeded_at_ms,
            current_target: flags.current.as_ref() == Some(&self.id),
            last_successful: flags.successful.as_ref() == Some(&self.id),
            git_sha: self.git_sha,
            git_branch: self.git_branch,
        })
    }
}

/// Which of an environment's deployments are its current target and its
/// last successful one.
struct Flags {
    current: Option<String>,
    successful: Option<String>,
}

impl Flags {
    /// Reads `environment`'s flags.
    async fn read_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &EnvironmentId,
    ) -> Result<Self, StoreError> {
        let id = environment.as_str();
        Ok(Self {
            current: Store::current_deployment_on(tx, environment)
                .await?
                .map(|current| current.id),
            successful: sqlx::query_scalar!("SELECT d.id FROM deployments d JOIN operations o ON o.id=d.id WHERE o.environment_id=?1 AND d.succeeded_at_ms IS NOT NULL ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1",id).fetch_optional(&mut **tx).await.map_err(StoreError::database)?.flatten(),
        })
    }
}

/// A deployment's captured inputs.
pub struct Snapshot {
    /// The manifest as captured, with references unresolved.
    pub template: ApplicationTemplate,
    /// The rendered manifest and its values; absent until a
    /// repository-backed manifest is fetched.
    pub rendering: Option<Rendering>,
    /// Problems found while fetching the manifest that did not stop the deployment.
    pub warnings: Vec<DiagnosticView>,
}

impl DeploymentRow {
    /// Decodes the captured columns.
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        let application: Option<NormalizedApplication> =
            serde_json::from_str(&self.manifest_json).map_err(StoreError::corrupt)?;
        let values: BTreeMap<String, VariableValue> =
            serde_json::from_str(&self.variables_json).map_err(StoreError::corrupt)?;
        Ok(Snapshot {
            template: serde_json::from_str(&self.template_json).map_err(StoreError::corrupt)?,
            rendering: application.map(|application| Rendering {
                application,
                values,
            }),
            warnings: serde_json::from_str(&self.warnings_json).map_err(StoreError::corrupt)?,
        })
    }
}

impl DeploymentRow {
    /// Decodes the row into the deployment's whole snapshot.
    async fn view_on(
        self,
        tx: &mut Transaction<'_, Sqlite>,
        flags: &Flags,
    ) -> Result<DeploymentView, StoreError> {
        let snapshot = self.snapshot()?;
        let (application, variables) = snapshot.rendering.map_or_else(
            || (None, BTreeMap::new()),
            |rendering| (Some(rendering.application), rendering.values),
        );
        let summary = SummaryRow {
            id: self.id,
            succeeded_at_ms: self.succeeded_at_ms,
            release_id: self.release_id,
            origin_json: self.origin_json,
            git_sha: None,
            git_branch: None,
        }
        .summary_on(tx, flags)
        .await?;
        Ok(DeploymentView {
            operation: summary.operation,
            template: snapshot.template,
            variables,
            application,
            warnings: snapshot.warnings,
            release: summary.release,
            origin: summary.origin,
            succeeded_at_ms: summary.succeeded_at_ms,
            current_target: summary.current_target,
            last_successful: summary.last_successful,
        })
    }
}

/// An environment's current deployment: the latest one whose target was
/// published, which its runtime runs.
pub struct CurrentDeployment {
    /// The deployment's ID, also its operation's.
    pub id: String,
    /// Whether its operation has succeeded, as opposed to still rolling out,
    /// failed, or retried.
    pub succeeded: bool,
    /// The release its target runs, when it recorded or received one.
    pub release: Option<piqueld_core::ReleaseId>,
}

impl Store {
    /// Reads `environment`'s current deployment, if any: its newest published
    /// one by creation time. IDs only order pages of the deployment list,
    /// because they are monotonic only within one daemon process.
    pub(super) async fn current_deployment_on(
        connection: &mut sqlx::SqliteConnection,
        environment: &EnvironmentId,
    ) -> Result<Option<CurrentDeployment>, StoreError> {
        let id = environment.as_str();
        sqlx::query!(
            r#"SELECT d.id AS "id!",d.release_id,o.state AS "state!" FROM deployments d JOIN operations o ON o.id=d.id WHERE o.environment_id=?1 AND o.promoted=1 ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1"#,
            id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(CurrentDeployment {
                succeeded: row.state == super::OperationState::Succeeded.as_str(),
                release: row
                    .release_id
                    .map(piqueld_core::ReleaseId::parse)
                    .transpose()
                    .map_err(StoreError::corrupt)?,
                id: row.id,
            })
        })
        .transpose()
    }

    /// See [`Self::current_deployment_on`].
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn current_deployment(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Option<CurrentDeployment>, StoreError> {
        Self::current_deployment_on(&mut *self.connection().await?, environment).await
    }

    /// Saves edited configuration with the next generation without creating an
    /// operation, so nothing is deployed until requested. Converges the
    /// environments to the ones it declares (see `converge_environments_on`);
    /// new applications that declare none get a `production` environment.
    /// New environments start as `not_deployed`. Returns
    /// `IllegalTransition` while deletion is pending, `AlreadyExists` for a taken
    /// name, `SecretDeleting` for manifests that reference secrets being deleted,
    /// and `secret_name_conflict` for declared secrets the application stores.
    /// The manifest an application last read from its repository is kept
    /// while it stays repository-backed (see `Self::repository_manifest_on`).
    pub(super) async fn save_configuration_on(
        tx: &mut Transaction<'_, Sqlite>,
        app: &ApplicationTemplate,
        expected: Option<u64>,
    ) -> Result<SavedApplication, StoreError> {
        let id = app.id().as_str();
        let mounted = app
            .spec()
            .secret_mounts()
            .filter_map(|secret| secret.name.as_literal())
            .collect::<Vec<_>>();
        Self::check_secret_references(
            tx,
            &mounted.iter().map(String::as_str).collect(),
            super::secret::SecretScope::Application(app.id()),
        )
        .await?;
        Self::check_declared_against_store_on(tx, id, &app.spec().secrets).await?;
        let current = Self::application_on(tx, id)
            .await?
            .map(|current| current.application);
        let previous_url = current
            .as_ref()
            .and_then(|current| Some(current.spec().manifest.as_ref()?.repository.url.clone()));
        let previous = Self::generation_on(tx, id, expected).await?;
        let generation = previous.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let json = serde_json::to_string(app).map_err(StoreError::corrupt)?;
        let name = app.metadata().name.as_str();
        let now = now_ms();
        let backed = app.spec().manifest.is_some();
        let changed = sqlx::query!("INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(id) DO UPDATE SET desired_json=excluded.desired_json,repository_json=CASE WHEN ?6 THEN applications.repository_json END,generation=excluded.generation,updated_at_ms=excluded.updated_at_ms WHERE applications.delete_intent=0",id,name,json,generation,now,backed)
            .execute(&mut **tx).await.map_err(StoreError::constraint)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let declarations = app
            .spec()
            .declared_environments_since(current.as_ref().map(ApplicationTemplate::spec))?;
        if previous == 0 && declarations.is_empty() {
            Self::create_default_environment_on(tx, app, now).await?;
        }
        Self::follow_connection_on(tx, app, previous_url.as_deref()).await?;
        Self::converge_environments_on(tx, app.id(), &declarations, None, now).await?;
        Ok(SavedApplication {
            application_id: id.into(),
            generation: u64::try_from(generation).map_err(StoreError::corrupt)?,
            operation_id: None,
            operation_ids: Vec::new(),
        })
    }

    /// Snapshots the manifest a tracking `environment` builds next (see
    /// [`super::Tracking::candidate`]) as operation `id`'s deployment
    /// record, rendered for that environment, together with the values its
    /// references resolved to, so retries never read variables again. A
    /// reference without a value, or a stored secret the environment may not
    /// mount (`SecretAccessDenied`), fails the request. Repository-backed
    /// manifests are rendered and checked once fetched, when their revision is known.
    /// The deployment row shares the operation's ID. A promoted environment
    /// fails with `environment_promoted`: it never builds.
    pub(super) async fn capture_deployment(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        environment: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let environment = Self::existing_environment_on(tx, environment.as_str()).await?;
        let template = environment.tracking()?.candidate(None)?;
        let rendering = if template.spec().manifest.is_some() {
            None
        } else {
            let rendering = environment.render(&template, id)?;
            Self::check_secret_access_on(tx, &environment.environment, &rendering.application)
                .await?;
            Some(rendering)
        };
        let (manifest, variables) = Self::snapshot_json(rendering.as_ref())?;
        let template = template.canonical_json().map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO deployments(id,environment_id,manifest_json,template_json,variables_json,generation,created_at_ms) SELECT id,environment_id,?2,?3,?4,generation,created_at_ms FROM operations WHERE id=?1",id,manifest,template,variables)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// The rendered manifest (JSON null when absent) and values columns.
    pub(super) fn snapshot_json(
        rendering: Option<&Rendering>,
    ) -> Result<(String, String), StoreError> {
        let manifest = rendering
            .map(|rendering| rendering.application.canonical_json())
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_else(|| "null".into());
        let variables = rendering
            .map(|rendering| serde_json::to_string(&rendering.values))
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_else(|| "{}".into());
        Ok((manifest, variables))
    }

    /// Where deployment `id` comes from: a build, or a release.
    /// # Errors
    /// Returns storage, decoding, or absence errors.
    pub async fn deployment_origin(&self, id: &str) -> Result<DeploymentOrigin, StoreError> {
        let origin = sqlx::query_scalar!("SELECT origin_json FROM deployments WHERE id=?1", id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
        serde_json::from_str(&origin).map_err(StoreError::corrupt)
    }

    /// Makes deployment `id` build every Git source, even one whose image
    /// it could reuse (see [`Self::reusable_builds`]).
    pub(super) async fn request_rebuild_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!("UPDATE deployments SET rebuild=1 WHERE id=?1", id)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }

    /// Whether deployment `id` was requested with a rebuild.
    /// # Errors
    /// Returns storage or absence errors.
    pub async fn rebuild_requested(&self, id: &str) -> Result<bool, StoreError> {
        Ok(
            sqlx::query_scalar!("SELECT rebuild FROM deployments WHERE id=?1", id)
                .fetch_optional(&self.pool)
                .await
                .map_err(StoreError::database)?
                .ok_or(StoreError::NotFound)?
                != 0,
        )
    }

    /// Whether deployment `id` ever succeeded.
    /// # Errors
    /// Returns storage or absence errors.
    pub async fn deployment_succeeded(&self, id: &str) -> Result<bool, StoreError> {
        Ok(
            sqlx::query_scalar!("SELECT succeeded_at_ms FROM deployments WHERE id=?1", id)
                .fetch_optional(&self.pool)
                .await
                .map_err(StoreError::database)?
                .ok_or(StoreError::NotFound)?
                .is_some(),
        )
    }

    /// Reads the inputs captured for an execution, never the editable application.
    /// # Errors
    /// Returns storage, decoding, or absence errors.
    pub async fn deployment_snapshot(&self, id: &str) -> Result<Snapshot, StoreError> {
        sqlx::query_as!(
            DeploymentRow,
            r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms,release_id,origin_json FROM deployments WHERE id=?1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?
        .snapshot()
    }

    /// Loads operation `id` and records its current attempt outcome.
    pub(super) async fn record_deployment_attempt(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<(), StoreError> {
        let op = Self::operation_on(tx, id).await?;
        Self::save_deployment_attempt(tx, &op).await
    }

    /// Upserts the outcome of the operation's current attempt in deployment
    /// history, and stamps the deployment's first success time when it succeeded.
    /// Operations without a deployment row (deletes) record nothing.
    pub(super) async fn save_deployment_attempt(
        tx: &mut Transaction<'_, Sqlite>,
        op: &Operation,
    ) -> Result<(), StoreError> {
        let id = &op.id;
        let json = serde_json::to_string(op).map_err(StoreError::corrupt)?;
        let attempt = i64::try_from(op.attempt).map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO deployment_attempts(deployment_id,attempt,outcome_json) SELECT id,?1,?2 FROM deployments WHERE id=?3 ON CONFLICT(deployment_id,attempt) DO UPDATE SET outcome_json=excluded.outcome_json",attempt,json,id).execute(&mut **tx).await.map_err(StoreError::database)?;
        if op.state == super::OperationState::Succeeded {
            sqlx::query!(
                "UPDATE deployments SET succeeded_at_ms=COALESCE(succeeded_at_ms,?1) WHERE id=?2",
                op.finished_at_ms,
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }

    /// Lists deployment snapshots newest first; history is retained until environment deletion.
    /// # Errors
    /// Returns storage, decoding, absence, or invalid cursor errors.
    pub async fn deployments(
        &self,
        app: &EnvironmentId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<DeploymentView>, StoreError> {
        self.get(app).await?;
        let paging = Paging::new(limit)?;
        let before = Paging::cursor(cursor)?.unwrap_or(NEWEST);
        let app_id = app.as_str();
        let mut tx = self.snapshot().await?;
        let mut rows = sqlx::query_as!(
            DeploymentRow,
            r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms,release_id,origin_json FROM deployments
             WHERE environment_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3"#,
            app_id,
            before,
            paging.fetch
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.id.clone());
        let flags = Flags::read_on(&mut tx, app).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.view_on(&mut tx, &flags).await?);
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Page { items, next_cursor })
    }

    /// Reads deployment `id` of environment `app`.
    /// # Errors
    /// Returns storage or decoding errors, and `NotFound` when `app` has no
    /// such deployment.
    pub async fn deployment(
        &self,
        app: &EnvironmentId,
        id: &str,
    ) -> Result<DeploymentView, StoreError> {
        let app_id = app.as_str();
        let mut tx = self.snapshot().await?;
        let row = sqlx::query_as!(
            DeploymentRow,
            r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms,release_id,origin_json FROM deployments
             WHERE environment_id=?1 AND id=?2"#,
            app_id,
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let flags = Flags::read_on(&mut tx, app).await?;
        let view = row.view_on(&mut tx, &flags).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(view)
    }

    /// Lists deployments like [`Self::deployments`], as summaries: without
    /// their manifests, which are never read.
    /// # Errors
    /// Returns storage, decoding, absence, or invalid cursor errors.
    pub async fn deployment_summaries(
        &self,
        app: &EnvironmentId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<DeploymentSummary>, StoreError> {
        self.get(app).await?;
        let paging = Paging::new(limit)?;
        let before = Paging::cursor(cursor)?.unwrap_or(NEWEST);
        let app_id = app.as_str();
        let mut tx = self.snapshot().await?;
        let mut rows = sqlx::query_as!(
            SummaryRow,
            r#"SELECT id AS "id!",succeeded_at_ms,release_id,origin_json,
             variables_json->>'$."git.sha"' AS "git_sha: String",
             variables_json->>'$."git.branch"' AS "git_branch: String"
             FROM deployments WHERE environment_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3"#,
            app_id,
            before,
            paging.fetch
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.id.clone());
        let flags = Flags::read_on(&mut tx, app).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.summary_on(&mut tx, &flags).await?);
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Page { items, next_cursor })
    }

    /// Lists durable attempt outcomes, including failures followed by successful retries.
    /// # Errors
    /// Returns storage, decoding or pagination errors.
    pub async fn deployment_attempts(
        &self,
        id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Operation>, StoreError> {
        let before = Paging::cursor(cursor)?
            .map(str::parse::<i64>)
            .transpose()
            .map_err(StoreError::invalid_input)?
            .unwrap_or(i64::MAX);
        let paging = Paging::new(limit)?;
        let mut rows = sqlx::query!(
            "SELECT attempt,outcome_json FROM deployment_attempts WHERE deployment_id=?1 AND attempt<?2 ORDER BY attempt DESC LIMIT ?3",
            id,
            before,
            paging.fetch
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.attempt);
        let items = rows
            .into_iter()
            .map(|row| serde_json::from_str(&row.outcome_json).map_err(StoreError::corrupt))
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_cursor })
    }
}

#[cfg(test)]
mod tests;
