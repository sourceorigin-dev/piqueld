//! Saved configuration and immutable deployment inputs. Saving never creates work.
use super::{EnvironmentId, NormalizedApplication, Operation, Paging, Store, StoreError, now_ms};
use piqueld_core::{
    api::{DeploymentOrigin, DeploymentView, DiagnosticView, Page, SavedApplication},
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
        Ok(DeploymentView {
            operation: Store::operation_on(tx, &self.id).await?,
            template: snapshot.template,
            variables,
            application,
            warnings: snapshot.warnings,
            release: self
                .release_id
                .map(piqueld_core::ReleaseId::parse)
                .transpose()
                .map_err(StoreError::corrupt)?,
            origin: serde_json::from_str(&self.origin_json).map_err(StoreError::corrupt)?,
            succeeded_at_ms: self.succeeded_at_ms,
            current_target: flags.current.as_ref() == Some(&self.id),
            last_successful: flags.successful.as_ref() == Some(&self.id),
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
        // What sync followed in another repository says nothing about this one.
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
        let before = Paging::cursor(cursor)?;
        let app_id = app.as_str();
        let mut tx = self.snapshot().await?;
        // Separate queries, so a cursor seeks `deployment_environment`.
        let mut rows = if let Some(before) = before {
            sqlx::query_as!(
                DeploymentRow,
                r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms,release_id,origin_json FROM deployments
                 WHERE environment_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3"#,
                app_id,
                before,
                paging.fetch
            )
            .fetch_all(&mut *tx)
            .await
        } else {
            sqlx::query_as!(
                DeploymentRow,
                r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms,release_id,origin_json FROM deployments
                 WHERE environment_id=?1 ORDER BY id DESC LIMIT ?2"#,
                app_id,
                paging.fetch
            )
            .fetch_all(&mut *tx)
            .await
        }
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
mod tests {
    use super::*;
    use crate::Actor::Daemon;
    use crate::{Mutation, MutationResponse};
    use piqueld_core::{ApplicationState, OperationState};

    fn empty() -> ApplicationTemplate {
        crate::testing::template("empty", "[spec]")
            .normalize(piqueld_core::ApplicationId::parse("app-empty-test").unwrap())
    }

    async fn save(store: &Store, app: ApplicationTemplate, generation: u64) -> SavedApplication {
        let (MutationResponse::Saved(saved), wake) = store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(app),
                    expected_application_id: None,
                    deploy: false,
                },
                Some(generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved response")
        };
        assert!(!wake);
        saved
    }

    /// Requests a deployment of `id`, returning it and whether the controller should wake.
    async fn deploy(
        store: &Store,
        id: &EnvironmentId,
        generation: u64,
        key: Option<&str>,
    ) -> (piqueld_core::api::AcceptedOperation, bool) {
        let (MutationResponse::Operation(operation), wake) = store
            .accept(
                Daemon,
                Mutation::deploy(id.clone()),
                Some(generation),
                false,
                key,
            )
            .await
            .unwrap()
        else {
            panic!("operation response")
        };
        (operation, wake)
    }

    #[tokio::test]
    async fn save_and_deploy_accepts_saved_only_configuration_but_cannot_reverse_deletion() {
        let (_temp, store) = Store::temporary().await;
        let saved = save(&store, empty(), 0).await;
        let id = store.saved_production(&saved).await;
        let application = store.get(&id).await.unwrap().application.application;
        let (MutationResponse::Saved(deployed), wake) = store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(application.clone()),
                    expected_application_id: Some(saved.application_id.clone()),
                    deploy: true,
                },
                Some(saved.generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved deployment")
        };
        assert!(wake);
        let operation_id = deployed.operation_id.unwrap();
        assert_eq!(
            store
                .deployment_snapshot(&operation_id)
                .await
                .unwrap()
                .template,
            application
        );
        let (MutationResponse::Deleted(deletion), _) = store
            .accept(
                Daemon,
                Mutation::DeleteApplication {
                    id: application.id().clone(),
                    environments: Vec::new(),
                },
                Some(deployed.generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("deleted")
        };
        for deploy in [false, true] {
            assert!(matches!(
                store
                    .accept(
                        Daemon,
                        Mutation::Save {
                            application: Box::new(application.clone()),
                            expected_application_id: Some(saved.application_id.clone()),
                            deploy,
                        },
                        Some(deletion.generation),
                        false,
                        None,
                    )
                    .await,
                Err(StoreError::IllegalTransition)
            ));
        }
        assert!(matches!(
            store
                .save_application(&application, None, Some(deletion.generation))
                .await,
            Err(StoreError::IllegalTransition)
        ));
        let current = store.get(&id).await.unwrap();
        assert!(current.delete_intent() && current.application.delete_intent);
        assert_eq!(current.application.generation, deletion.generation);
        assert_eq!(
            store
                .latest_operation_for_environment(&id)
                .await
                .unwrap()
                .unwrap()
                .id,
            deletion.operations[0].operation_id
        );
    }

    #[tokio::test]
    async fn saved_changes_cannot_enter_deployments_or_retries_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = Store::open_unsynced(&path).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = store.saved_production(&saved).await;
        assert_eq!(
            store.status(&id).await.unwrap().state,
            ApplicationState::NotDeployed
        );
        assert!(
            store
                .latest_operation_for_environment(&id)
                .await
                .unwrap()
                .is_none()
        );
        let (first, wake) = deploy(&store, &id, saved.generation, Some("deploy-once")).await;
        assert!(wake);
        let original = store
            .deployment_snapshot(&first.operation_id)
            .await
            .unwrap()
            .template;
        store.mark_running(&first.operation_id).await;
        let mut edited = original.to_manifest();
        edited.spec.volumes.push(piqueld_core::manifest::Volume {
            name: "later".into(),
        });
        let edited = edited
            .validate_template()
            .unwrap()
            .normalize(original.id().clone());
        let changed = save(&store, edited, 1).await;
        assert_eq!(changed.generation, 2);
        drop(store);
        let store = Store::open_unsynced(&path).await.unwrap();
        store.recover_interrupted().await.unwrap();
        let op = store.operation(&first.operation_id).await.unwrap();
        let attempts = store.deployment_attempts(&op.id, None, 100).await.unwrap();
        assert_eq!(attempts.items.len(), 1);
        assert_eq!(attempts.items[0].attempt, 1);
        assert_eq!(attempts.items[0].state, OperationState::Cancelled);
        assert!(attempts.items[0].finished_at_ms.is_some());
        assert_eq!(store.recover_interrupted().await.unwrap(), 0);
        assert_eq!(op.generation, 1);
        assert_eq!(
            store.deployment_snapshot(&op.id).await.unwrap().template,
            original
        );
        let (replay, wake) = deploy(&store, &id, 1, Some("deploy-once")).await;
        assert!(!wake);
        assert_eq!(replay.operation_id, op.id);
        let (next, _) = deploy(&store, &id, 2, None).await;
        assert_ne!(next.operation_id, op.id);
        assert!(matches!(
            store.retry_operation(&op).await,
            Err(StoreError::IllegalTransition)
        ));
        assert_eq!(
            store.deployments(&id, None, 3).await.unwrap().items.len(),
            2
        );
        assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failure_survives_success_and_empty_target_has_no_resources() {
        let (_temp, store) = Store::temporary().await;
        let saved = save(&store, empty(), 0).await;
        let id = store.saved_production(&saved).await;
        let op = store.request_deploy(&id, Some(1)).await.unwrap();
        let target = piqueld_core::compile_application(
            &store
                .deployment_snapshot(&op.id)
                .await
                .unwrap()
                .rendering
                .unwrap()
                .application,
            &id,
            store.instance_id().clone(),
            &piqueld_core::ResolutionSet::default(),
        )
        .unwrap();
        assert!(
            target.services.is_empty() && target.networks.is_empty() && target.volumes.is_empty()
        );
        store.mark_running(&op.id).await;
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("docker_unavailable", "Docker is unavailable")),
            )
            .await
            .unwrap();
        let op = store.operation(&op.id).await.unwrap();
        store.retry_operation(&op).await.unwrap();
        store.mark_running(&op.id).await;
        store.save_prepared(&op, &target).await.unwrap();
        store.publish_prepared(&op).await.unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        store.prune_events(i64::MAX).await.unwrap();
        let history = store.deployment_attempts(&op.id, None, 100).await.unwrap();
        assert_eq!(history.items.len(), 2);
        assert_eq!(
            history.items[1].error_code.as_deref(),
            Some("docker_unavailable")
        );
        let deployments = store.deployments(&id, None, 3).await.unwrap();
        assert!(deployments.items[0].current_target && deployments.items[0].last_successful);
    }

    /// Deployment IDs are monotonic only within one process, so after a
    /// restart with the clock behind, a newer deployment can sort below an
    /// older one: the current and last successful deployments are the
    /// newest by creation time, not by ID.
    #[tokio::test]
    async fn current_deployment_is_the_newest_by_creation_time_not_by_id() {
        let (_temp, store) = Store::temporary().await;
        let saved = store
            .save_template(crate::testing::template("alpha", "[spec]"), true)
            .await;
        let id = store.saved_production(&saved).await;
        let alpha = saved.operation_id.unwrap();
        // Published and succeeded, as a finished rollout leaves it.
        sqlx::query("UPDATE operations SET promoted=1,state='succeeded',finished_at_ms=created_at_ms WHERE id=?1")
            .bind(&alpha)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE deployments SET succeeded_at_ms=created_at_ms WHERE id=?1")
            .bind(&alpha)
            .execute(&store.pool)
            .await
            .unwrap();
        // An already-published deployment from before the restart: its ID
        // sorts above alpha's, but it was created a minute earlier.
        let older = format!("operation-{}", "f".repeat(32));
        sqlx::query("INSERT INTO operations(id,environment_id,kind,state,generation,promoted,created_at_ms,updated_at_ms,finished_at_ms) SELECT ?1,environment_id,kind,'succeeded',generation,1,created_at_ms-60000,created_at_ms-60000,created_at_ms-60000 FROM operations WHERE id=?2")
            .bind(&older)
            .bind(&alpha)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deployments(id,environment_id,generation,manifest_json,template_json,variables_json,created_at_ms,succeeded_at_ms) SELECT ?1,environment_id,generation,manifest_json,template_json,variables_json,created_at_ms-60000,created_at_ms-60000 FROM deployments WHERE id=?2")
            .bind(&older)
            .bind(&alpha)
            .execute(&store.pool)
            .await
            .unwrap();
        let latest = store.latest_operation_for_environment(&id).await.unwrap();
        assert_eq!(latest.unwrap().id, alpha);
        assert_eq!(
            store.current_deployment(&id).await.unwrap().unwrap().id,
            alpha
        );
        // Paging stays in ID order, so the older deployment lists first.
        let deployments = store.deployments(&id, None, 2).await.unwrap().items;
        let flags = deployments
            .iter()
            .map(|deployment| {
                (
                    deployment.operation.id.as_str(),
                    deployment.current_target,
                    deployment.last_successful,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            flags,
            [(older.as_str(), false, false), (alpha.as_str(), true, true)]
        );
    }
}
