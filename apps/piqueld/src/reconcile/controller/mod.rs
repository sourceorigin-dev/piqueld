//! Runs an operation to a durable outcome: executing its plan, publishing
//! routes and cleaning up.
use super::{
    ApplicationState, BlockedPlan, CancellationToken, Controller, DockerApi, Operation,
    OperationError, OperationKind, OperationState, Plan, PlanRequest, StoreError,
};
use crate::application::RuntimeBoundary;
use piqueld_core::Phase;
use piqueld_core::api::DeploymentOrigin;
use piqueld_core::timeline::{PlanCoverage, StepKey};
use piqueld_docker::JobRuns;
use std::sync::Arc;

impl<D: DockerApi> Controller<D> {
    /// Executes an operation to a durable outcome, then closes any journal actions
    /// it left open as `action_outcome_unknown`. Only store failures are returned;
    /// operation failures are persisted on the operation itself.
    #[tracing::instrument(skip_all, fields(
        environment_id = %operation.environment_id,
        operation_id = %operation.id,
        generation = operation.generation,
        operation_kind = ?operation.kind,
    ))]
    pub(super) async fn run_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), StoreError> {
        let started = std::time::Instant::now();
        tracing::info!("operation started");
        let result = {
            let _in_use = self.images_in_use().await;
            self.run_operation_inner(operation, cancellation).await
        };
        self.images.request_cleanup();
        {
            let _guard = self.mutations.lock().await;
            self.store.interrupt_actions(Some(&operation.id)).await?;
        }
        let duration_ms = started.elapsed().as_secs_f64() * 1_000.0;
        match &result {
            Ok(outcome) => tracing::info!(outcome, duration_ms, "operation execution completed"),
            Err(error) => tracing::error!(?error, duration_ms, "operation journal update failed"),
        }
        result.map(|_| ())
    }

    /// Moves the operation to `Running`, executes it, and persists the outcome.
    ///
    /// A `Requested` operation that can no longer start is reported as
    /// superseded, and a `Running` one with a recorded transient error is cycled
    /// through `Requested` for a fresh attempt. A cancelled local token persists
    /// nothing. Otherwise success completes the operation (or finalizes
    /// deletion), a `Cancelled`/`Superseded` error defers to newer durable state,
    /// a failed deletion keeps running with the error recorded, and other
    /// failures mark the environment degraded and the operation failed, or roll
    /// a failed rollout back (see `rollback`). Returns a short outcome label.
    async fn run_operation_inner(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<&'static str, StoreError> {
        if operation.state == OperationState::Requested {
            match self
                .store
                .transition_operation(
                    &operation.id,
                    OperationState::Requested,
                    OperationState::Running,
                    None,
                )
                .await
            {
                Ok(()) => {}
                Err(StoreError::IllegalTransition) => return Ok("superseded"),
                Err(error) => return Err(error),
            }
        }
        if operation.state == OperationState::Running && operation.error_code.is_some() {
            self.store
                .transition_operation(
                    &operation.id,
                    OperationState::Running,
                    OperationState::Requested,
                    None,
                )
                .await?;
            self.store
                .transition_operation(
                    &operation.id,
                    OperationState::Requested,
                    OperationState::Running,
                    None,
                )
                .await?;
        }
        let operation = &self.store.operation(&operation.id).await?;
        let result = self.execute_and_cleanup(operation, cancellation).await;
        if cancellation.is_cancelled() {
            return Ok("cancelled");
        }
        let outcome = match &result {
            Ok(_) => "succeeded",
            Err(error) => error.code(),
        };
        let persisted = match result {
            Ok(_) if operation.kind == OperationKind::Delete => {
                self.store.finish_delete_operation(operation).await
            }
            Ok(stopped) => {
                self.store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Succeeded,
                        None,
                    )
                    .await?;
                if self.store.is_stopped(&operation.environment_id).await? != stopped {
                    self.store
                        .request_reconcile(&operation.environment_id, &operation.id)
                        .await?;
                }
                Ok(())
            }
            Err(OperationError::Cancelled | OperationError::Superseded) => {
                match self
                    .store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Cancelled,
                        None,
                    )
                    .await
                {
                    Ok(()) | Err(StoreError::IllegalTransition) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Err(error) if operation.kind == OperationKind::Delete => {
                tracing::warn!(code = error.code(), error = ?error, "deletion will be retried");
                self.store
                    .record_operation_error(operation, error.code(), &error.message())
                    .await
            }
            Err(error) => {
                tracing::error!(code = error.code(), error = ?error, "operation failed");
                self.fail(operation, &error).await
            }
        };
        persisted.map(|()| outcome)
    }

    /// Completes convergence, then removes the configs of files no service
    /// mounts any more, whether or not it succeeded, and after service
    /// deletion retained secrets and a deleted preview's volumes
    /// (environments have no volume inventory, so they keep theirs). The
    /// environment's next operation waits for this one, and repair for the
    /// mutation lock, so nothing publishes configs meanwhile. A failed removal is only logged, and left
    /// to the next operation, except that a deletion is retried. Returns
    /// whether the environment was stopped as the final plan saw it.
    async fn execute_and_cleanup(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<bool, OperationError> {
        let executed = Box::pin(self.execute_operation(operation, cancellation)).await;
        if cancellation.is_cancelled() {
            return executed;
        }
        let pruned = {
            let _guard = self.mutations.lock().await;
            self.docker
                .prune_files(&self.ownership_labels(&operation.environment_id))
                .await
        };
        let stopped = executed?;
        match pruned {
            Err(error) if operation.kind == OperationKind::Delete => return Err(error.into()),
            Err(error) => tracing::warn!(?error, "removing unused service files failed"),
            Ok(()) => {}
        }
        if operation.kind == OperationKind::Delete {
            let names = self.store.secret_names(&operation.environment_id).await?;
            if !names.is_empty() {
                self.remove_secrets(operation, &names).await?;
            }
            Box::pin(self.remove_preview_volumes(operation)).await?;
            let observed = self.docker.observe(&operation.environment_id).await?;
            Box::pin(self.enforce_links(&operation.environment_id, &operation.id, &observed))
                .await?;
            self.enforce_telemetry(&operation.environment_id).await?;
        }
        Ok(stopped)
    }

    /// Removes every volume in a preview's inventory, including volumes later
    /// manifests dropped, journaled like other runtime mutations. Then
    /// observes the runtime and fails, so the deletion is retried, while any
    /// of them is left.
    async fn remove_preview_volumes(&self, operation: &Operation) -> Result<(), OperationError> {
        let names = self
            .store
            .preview_volumes(&operation.environment_id)
            .await?;
        if names.is_empty() {
            return Ok(());
        }
        self.journaled(
            &operation.id,
            Phase::RemoveVolumes,
            None,
            self.remove_volumes(operation, &names),
        )
        .await?;
        Ok(self.store.mutation_event(&operation.id).await?)
    }

    /// Removes `names` and verifies none of them is still observed.
    async fn remove_volumes(
        &self,
        operation: &Operation,
        names: &[String],
    ) -> Result<(), OperationError> {
        let ownership = self.ownership_labels(&operation.environment_id);
        for name in names {
            self.docker.remove_volume(name, &ownership).await?;
        }
        let observed = self.docker.observe(&operation.environment_id).await?;
        if observed
            .volumes
            .iter()
            .any(|volume| names.contains(&volume.name))
        {
            return Err(OperationError::DockerRequestFailed(
                "verifying the preview's volumes were removed",
            ));
        }
        Ok(())
    }

    /// Journals retained secret removal like other runtime mutations.
    async fn remove_secrets(
        &self,
        operation: &Operation,
        names: &[String],
    ) -> Result<(), OperationError> {
        let ownership = self.ownership_labels(&operation.environment_id);
        self.journaled(&operation.id, Phase::RemoveSecrets, None, async {
            Ok(self.docker.remove_secrets(names, &ownership).await?)
        })
        .await?;
        Ok(self.store.mutation_event(&operation.id).await?)
    }

    /// Plans from fresh observations until no work remains. Only desired state and
    /// operation status are durable; Docker state determines the next action.
    ///
    /// The convergence deadline bounds time without progress: it restarts each
    /// time a service converges, so a dependency chain gets the full timeout per
    /// link while a single stuck service still times out. Returns whether the
    /// environment was stopped as the final plan saw it.
    #[tracing::instrument(skip_all, fields(phase = "convergence"))]
    async fn execute_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<bool, OperationError> {
        let request = Box::pin(self.operation_request(operation, cancellation)).await?;
        let ownership = self.ownership_labels(&operation.environment_id);
        if operation.kind != OperationKind::Delete
            && !self
                .store
                .set_status_for_operation(&operation.id, ApplicationState::Deploying, None)
                .await?
        {
            return Err(OperationError::Superseded);
        }
        if let PlanRequest::Reconcile { desired } = &request {
            self.store
                .warn_ungranted_receiver(&operation.id, desired)
                .await?;
            self.run_jobs(operation, desired, cancellation).await?;
        }
        let mut deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        if operation.kind == OperationKind::Delete {
            self.withdraw_routes(operation, deadline).await?;
            self.remove_jobs(operation, &ownership, JobRuns::All, cancellation)
                .await?;
        }
        tracing::debug!(
            timeout_seconds = self.retry.convergence_timeout.as_secs(),
            "convergence started"
        );
        let mut recorded: Option<Vec<StepKey>> = None;
        loop {
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(OperationError::ConvergenceTimeout);
            }
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
            self.check_current(operation).await?;
            self.store.record_health(&operation.id, &observed).await?;
            let accepted_routes = self.store.applied_routes(&operation.environment_id).await?;
            let stopped = self.store.is_stopped(&operation.environment_id).await?;
            let runtime_request = match &request {
                PlanRequest::Reconcile { desired } => PlanRequest::Reconcile {
                    desired: self
                        .runtime_target(desired, &accepted_routes, &observed, stopped)
                        .await?,
                },
                _ => request.clone(),
            };
            let plan = Plan::from_request(&runtime_request, &observed);
            self.check_plan(operation, &plan).await?;
            let steps = plan.steps();
            if recorded.as_ref() != Some(&steps) {
                self.store
                    .record_plan(&operation.id, steps.clone(), PlanCoverage::Runtime, None)
                    .await?;
                recorded = Some(steps);
            }
            if operation.kind != OperationKind::Delete {
                self.publish(operation, &request, &plan, deadline).await?;
            }
            if self.store.applied_routes(&operation.environment_id).await? != accepted_routes {
                continue;
            }
            let action = plan.actions.iter().find(|action| action.kind.runs());
            let Some(action) = action else {
                if operation.kind != OperationKind::Delete
                    && !self
                        .store
                        .set_status_for_operation(&operation.id, ApplicationState::Ready, None)
                        .await?
                {
                    return Err(OperationError::Superseded);
                }
                return Ok(stopped);
            };
            self.execute_action(
                action,
                operation,
                &ownership,
                stopped,
                cancellation,
                deadline,
            )
            .await?;
            if let Some(steps) = &mut recorded
                && !steps.is_empty()
            {
                steps.remove(0);
            }
            if matches!(action.kind, piqueld_core::ActionKind::WaitForService { .. }) {
                deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
            }
        }
    }

    /// Publishes `operation`'s prepared target, then stages the routes of
    /// `request`'s target, ready once `plan`'s resources are, within
    /// `deadline`.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    async fn publish(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        plan: &Plan,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        {
            let _guard = self.mutations.lock().await;
            self.check_current(operation).await?;
            self.store.publish_prepared(operation).await?;
        }
        if let PlanRequest::Reconcile { desired } = request {
            let ready = plan.desired_resources_ready();
            tokio::time::timeout_at(
                deadline,
                self.sync_routes(operation, &desired.routes, ready),
            )
            .await
            .map_err(|_| OperationError::ConvergenceTimeout)??;
        }
        Ok(())
    }

    /// Public exposure must be withdrawn within the deletion's convergence budget.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    async fn withdraw_routes(
        &self,
        operation: &Operation,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        tokio::time::timeout_at(deadline, async {
            self.check_current(operation).await?;
            self.sync_routes(operation, &[], true).await
        })
        .await
        .map_err(|_| OperationError::ConvergenceTimeout)?
    }

    /// Builds the plan request for an operation: a deletion request, or a
    /// reconcile request toward a freshly prepared target. Preparation is bounded
    /// by `prepare_timeout` and aborts on cancellation.
    #[expect(
        clippy::map_err_ignore,
        reason = "`Elapsed` carries nothing the timeout variant does not already say"
    )]
    async fn operation_request(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<PlanRequest, OperationError> {
        let application = self.store.get(&operation.environment_id).await?;
        if operation.kind == OperationKind::Delete {
            return Ok(self.delete_request(&operation.environment_id));
        }
        let prepared = tokio::time::timeout(
            self.prepare_timeout,
            self.prepare_target(operation, &application),
        );
        let desired = tokio::select! {
            () = cancellation.cancelled() => return Err(OperationError::Cancelled),
            result = prepared => result.map_err(|_| OperationError::PreparationTimeout)??,
        };
        Ok(PlanRequest::Reconcile { desired })
    }

    /// Stages the environment's routes and applies them through managed ingress
    /// when present (which skips the gateway while backends are not ready and no
    /// hostname is withdrawn), or acknowledges the routing table directly.
    /// `ready` tells staging whether the backing services exist yet. No-op when
    /// there are neither desired nor stored routes.
    async fn sync_routes(
        &self,
        operation: &Operation,
        routes: &[piqueld_core::manifest::ValidatedRoute],
        ready: bool,
    ) -> Result<(), OperationError> {
        if routes.is_empty() && !self.store.has_routes(&operation.environment_id).await? {
            return Ok(());
        }
        let staging = async {
            if let Some(ingress) = &self.ingress {
                Box::pin(ingress.apply(operation, routes, ready))
                    .await
                    .map_err(|error| match error.downcast_ref::<StoreError>() {
                        Some(StoreError::IllegalTransition) => OperationError::Superseded,
                        _ => OperationError::Ingress(error),
                    })?;
            } else {
                self.store
                    .stage_routes(
                        &operation.environment_id,
                        routes,
                        ready,
                        Some(&operation.id),
                    )
                    .await
                    .map_err(|error| match error {
                        StoreError::IllegalTransition => OperationError::Superseded,
                        other => other.into(),
                    })?;
                let table = self.store.served_table().await?;
                self.store.acknowledge_routes(&table).await?;
            }
            Ok(())
        };
        if !self.store.routed(operation).await?
            || (ready && self.store.applied_routes(&operation.environment_id).await? != routes)
        {
            self.journaled(&operation.id, Phase::Routing, None, staging)
                .await
        } else {
            staging.await
        }
    }

    /// Rejects a blocked plan with its classified error, after recording it
    /// with the first resource that blocks it, which the operation's phase
    /// then names.
    pub(super) async fn check_plan(
        &self,
        operation: &Operation,
        plan: &Plan,
    ) -> Result<(), OperationError> {
        tracing::debug!(
            actions = plan.actions.len(),
            blocked = plan.is_blocked(),
            "observation planned"
        );
        let Some(blocked_by) = plan
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.blocking)
        else {
            return Ok(());
        };
        self.store
            .record_plan(
                &operation.id,
                plan.steps(),
                PlanCoverage::Runtime,
                Some(blocked_by.resource.as_str()),
            )
            .await?;
        Err(BlockedPlan::of(plan).error())
    }

    /// Resolves the operation's deployable target, reusing a previously saved one.
    ///
    /// Verifies the swarm topology, then returns the saved target if there is
    /// one, as every release and restore has. A build's saved target needs the
    /// environment still tracking, unless it is already the active target.
    /// Otherwise it fetches the deployment manifest, keeps the application's
    /// current name, reuses prior image resolutions unless this is a `Refresh`,
    /// pins secret versions, then resolves images and builds sources. The result
    /// is saved only if the operation is still current and the topology still
    /// supported; image cleanup waits until then.
    #[tracing::instrument(skip_all, fields(phase = "preparation", timeout_seconds = self.prepare_timeout.as_secs()))]
    async fn prepare_target(
        &self,
        operation: &Operation,
        application: &super::StoredEnvironment,
    ) -> Result<piqueld_core::ResolvedApplication, OperationError> {
        self.check_current(operation).await?;
        self.docker.ensure_swarm(false).await?;
        let builds = matches!(
            self.store.deployment_origin(&operation.id).await?,
            DeploymentOrigin::Build | DeploymentOrigin::Local(_)
        );
        if let Some(target) = self.store.prepared_target(&operation.id).await? {
            if builds && !self.store.is_promoted(&operation.id).await? {
                application.tracking()?;
            }
            return Ok(target);
        }
        if !builds {
            return Err(StoreError::Corrupt.into());
        }
        let tracking = application.tracking()?;
        let runtime = crate::application::ApplicationRuntime::new(
            Arc::clone(&self.docker),
            &self.store,
            &self.sources,
            Arc::new(tokio::sync::Notify::new()),
            self.prepare_timeout,
            self.images_lock(),
            Arc::clone(&self.mutations),
        )
        .with_progress(Arc::clone(&self.store), operation.id.clone())
        .with_build_priority(&application.environment.kind)
        .with_rebuild(self.store.rebuild_requested(&operation.id).await?);
        let (manifest, snapshot) = Box::pin(self.deployment_manifest(operation, tracking)).await?;
        let runtime = runtime.with_snapshot(snapshot);
        let manifest =
            manifest.with_name(application.application.application.metadata().name.clone());
        let mut reusable = if operation.kind == OperationKind::Refresh {
            piqueld_core::ResolutionSet::default()
        } else {
            application
                .resolved
                .as_ref()
                .map_or_else(piqueld_core::ResolutionSet::default, |target| {
                    target.reusable_resolutions(&manifest)
                })
        };
        reusable.secret_names = self.store.pin_secrets(operation, &manifest).await?;
        let prepared = runtime
            .prepare(&operation.environment_id, &manifest, &reusable)
            .await
            .map_err(|error| match error {
                crate::application::BoundaryError::Store(error) => OperationError::from(error),
                crate::application::BoundaryError::Runtime(error) => OperationError::from(error),
                crate::application::BoundaryError::GitBuild(error) => {
                    tracing::warn!(error = ?error, "Git source build failed");
                    OperationError::GitBuildFailed(error)
                }
                crate::application::BoundaryError::ImageUnavailable(error) => *error,
                crate::application::BoundaryError::Compilation(errors) => {
                    tracing::error!(?errors, "application compilation failed");
                    OperationError::ValidationFailed("compile application")
                }
            })?;
        self.check_current(operation).await?;
        self.docker.ensure_swarm(false).await?;
        self.store
            .save_prepared(operation, &prepared)
            .await
            .map_err(OperationError::from)
    }

    /// Fails with `Superseded` when a newer operation exists for the environment,
    /// or `Cancelled` when this operation is no longer `Running`.
    pub(super) async fn check_current(&self, operation: &Operation) -> Result<(), OperationError> {
        let current = self
            .store
            .latest_operation_for_environment(&operation.environment_id)
            .await?;
        let Some(current) = current.filter(|current| current.id == operation.id) else {
            return Err(OperationError::Superseded);
        };
        if current.state != OperationState::Running {
            return Err(OperationError::Cancelled);
        }
        Ok(())
    }

    /// Marks the environment degraded and the running operation failed with
    /// `error`. A failed job fails its operation as soon as it is known, so
    /// failing it again finds it failed with that error already.
    pub(super) async fn fail_operation(
        &self,
        operation: &Operation,
        error: &OperationError,
    ) -> Result<(), StoreError> {
        self.store
            .set_status_for_operation(
                &operation.id,
                ApplicationState::Degraded,
                Some(&error.message()),
            )
            .await?;
        match self
            .store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Failed,
                Some((error.code(), &error.message())),
            )
            .await
        {
            Err(StoreError::IllegalTransition) => {
                let current = self.store.operation(&operation.id).await?;
                if current.state == OperationState::Failed
                    && current.error_code.as_deref() == Some(error.code())
                {
                    Ok(())
                } else {
                    Err(StoreError::IllegalTransition)
                }
            }
            persisted => persisted,
        }
    }
}

#[cfg(test)]
mod tests;
