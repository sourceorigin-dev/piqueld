//! Durable history, manifest export, runtime logs, and traffic.

use super::{ApplicationError, ApplicationService};
use piqueld_core::observability::EventFilter;
use piqueld_core::timeline::Timeline;
use piqueld_core::{
    ApplicationId, EnvironmentId, Operation, OperationState,
    api::{
        ApplicationLogs, BuildLogPage, BuildRecord, DeploymentListView, DeploymentPage,
        DeploymentView, LogStream, Page, ReleaseView,
    },
    observability::TrafficReport,
};
use piqueld_store::StoreError;
use piqueld_store::traffic::{DAY_MS, TrafficQuery};
use piqueld_store::{MAX_PAGE_SIZE, Visibility};

/// The longest traffic window a report covers.
const MAX_TRAFFIC_WINDOW_MS: i64 = 400 * DAY_MS;

/// Saved configuration rendered for download or export.
pub struct ManifestExport {
    /// Suggested filename, derived from the validated application name.
    pub filename: String,
    /// Saved manifest encoded as TOML.
    pub contents: String,
}

impl ApplicationService {
    /// Exports saved configuration without consulting Docker.
    /// # Errors
    /// Returns absence, storage, or TOML serialization errors.
    pub async fn manifest(&self, id: &ApplicationId) -> Result<ManifestExport, ApplicationError> {
        let application = self.store.application(id).await?.application;
        Ok(ManifestExport {
            filename: format!("{}.toml", application.metadata().name),
            contents: toml::to_string_pretty(&application.to_manifest())
                .map_err(ApplicationError::ManifestSerialization)?,
        })
    }

    /// Reads a durable operation and its progress.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn operation(&self, id: &str) -> Result<Operation, ApplicationError> {
        Ok(self.store.operation(id).await?)
    }

    /// Operation `id`'s timeline as of now: its steps, from its events, and
    /// what its environment's recent successful operations expect.
    /// # Errors
    /// Returns absence, expired history, or storage errors.
    pub async fn timeline(&self, id: &str) -> Result<Timeline, ApplicationError> {
        let operation = self.store.operation(id).await?;
        let baseline = self.store.timeline_baseline(&operation).await?;
        let mut timeline = Timeline::new(operation, baseline.forecast, baseline.expected);
        self.advance_timeline(&mut timeline).await?;
        Ok(timeline)
    }

    /// Applies the events of `timeline`'s operation newer than its cursor,
    /// reading the operation again when there were any. A timeline shows
    /// every step of an operation the caller may read, including failures
    /// of shared infrastructure that history only shows `system:read`. Once
    /// the operation is requested or runs again, the timeline is read anew,
    /// so a stream open across a rerun shows what a new read would.
    /// Returns whether it changed.
    /// # Errors
    /// Returns absence, expired history, or storage errors.
    pub async fn advance_timeline(
        &self,
        timeline: &mut Timeline,
    ) -> Result<bool, ApplicationError> {
        let filter = EventFilter {
            operation_id: Some(timeline.operation.id.clone()),
            ..EventFilter::default()
        };
        let from = timeline.cursor;
        loop {
            let (events, _) = self
                .store
                .stream_events(&filter, &Visibility::ALL, timeline.cursor, MAX_PAGE_SIZE)
                .await?;
            for event in &events {
                timeline.apply(event);
            }
            if events.len() < MAX_PAGE_SIZE {
                break;
            }
        }
        let advanced = timeline.cursor > from;
        if advanced {
            let operation = self.store.operation(&timeline.operation.id).await?;
            let requested = |operation: &Operation| operation.state == OperationState::Requested;
            if operation.attempt != timeline.operation.attempt
                || (requested(&operation) && !requested(&timeline.operation))
            {
                *timeline = Box::pin(self.timeline(&operation.id)).await?;
            } else {
                timeline.set_operation(operation);
            }
        }
        Ok(advanced)
    }

    /// Lists deployments, newest first, `limit` per page (up to
    /// `MAX_PAGE_SIZE`), three without a limit: whole snapshots, or
    /// summaries without manifests.
    /// # Errors
    /// Returns pagination, absence, or storage errors.
    pub async fn deployments(
        &self,
        id: &EnvironmentId,
        cursor: Option<&str>,
        limit: Option<u16>,
        view: DeploymentListView,
    ) -> Result<DeploymentPage, ApplicationError> {
        let limit = limit.unwrap_or(3);
        if !(1..=piqueld_core::api::MAX_PAGE_SIZE).contains(&limit) {
            return Err(ApplicationError::InvalidPagination);
        }
        let limit = usize::from(limit);
        Ok(match view {
            DeploymentListView::Full => {
                DeploymentPage::Full(self.store.deployments(id, cursor, limit).await?)
            }
            DeploymentListView::Summary => {
                DeploymentPage::Summary(self.store.deployment_summaries(id, cursor, limit).await?)
            }
        })
    }

    /// Reads one deployment of environment `id`.
    /// # Errors
    /// Returns absence, when `id` has no such deployment, or storage errors.
    pub async fn deployment(
        &self,
        id: &EnvironmentId,
        deployment: &str,
    ) -> Result<DeploymentView, ApplicationError> {
        Ok(self.store.deployment(id, deployment).await?)
    }

    /// Lists an application's releases, newest first, twenty per page, with
    /// whether each one's images are still present. Availability is left out
    /// when Docker can't be asked.
    /// # Errors
    /// Returns pagination, absence, or storage errors.
    pub async fn releases(
        &self,
        id: &ApplicationId,
        cursor: Option<&str>,
    ) -> Result<Page<ReleaseView>, ApplicationError> {
        let mut page = self.store.releases(id, cursor, 20).await?;
        self.mark_availability(&mut page.items).await;
        Ok(page)
    }

    /// Records whether each of `releases`' images are still present, leaving
    /// availability out when Docker can't be asked.
    pub(super) async fn mark_availability(
        &self,
        releases: impl IntoIterator<Item = &mut ReleaseView>,
    ) {
        match self.runtime.local_images().await {
            Ok(present) => {
                for release in releases {
                    release.availability = Some(present.availability(release.release.sources()));
                }
            }
            Err(error) => tracing::warn!(?error, "release availability unknown"),
        }
    }

    /// Lists retained attempts for a deployment owned by this environment.
    /// # Errors
    /// Returns pagination, absence, ownership, or storage errors.
    pub async fn deployment_attempts(
        &self,
        id: &EnvironmentId,
        deployment: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Operation>, ApplicationError> {
        let operation = self.store.operation(deployment).await?;
        if operation.environment_id != *id {
            return Err(StoreError::NotFound.into());
        }
        self.store.deployment_snapshot(deployment).await?;
        Ok(self
            .store
            .deployment_attempts(deployment, cursor, MAX_PAGE_SIZE)
            .await?)
    }

    /// Lists build history of applications within `visible`, newest first.
    /// # Errors
    /// Returns pagination or storage errors.
    pub async fn builds(
        &self,
        application: Option<&ApplicationId>,
        environment: Option<&EnvironmentId>,
        visible: &piqueld_core::access::Scope,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<BuildRecord>, ApplicationError> {
        Ok(self
            .store
            .builds(application, environment, visible, cursor, limit)
            .await?)
    }

    /// Returns the owner of the environment a build attempt belongs to, if
    /// the build exists.
    /// # Errors
    /// Returns storage errors.
    pub async fn build_owner(
        &self,
        id: i64,
    ) -> Result<Option<piqueld_store::EnvironmentOwner>, ApplicationError> {
        Ok(self.store.build_owner(id).await?)
    }

    /// Reads the newest bounded page of persisted build output (chunks in
    /// chronological order). `before` is the exclusive `previous_offset` cursor
    /// of a previous page, for loading older output.
    /// # Errors
    /// Returns invalid query, absence, or storage errors.
    pub async fn build_logs(
        &self,
        id: i64,
        before: Option<i64>,
        stream: Option<LogStream>,
    ) -> Result<BuildLogPage, ApplicationError> {
        Ok(self.store.build_logs(id, before, stream).await?)
    }

    /// Environment `id`'s traffic over a window of at most
    /// [`MAX_TRAFFIC_WINDOW_MS`], and whether the gateway counts it now.
    /// # Errors
    /// Returns invalid query, absence, or storage errors.
    pub async fn traffic(
        &self,
        id: &EnvironmentId,
        query: TrafficQuery<'_>,
    ) -> Result<TrafficReport, ApplicationError> {
        let length = query.until_ms.checked_sub(query.since_ms);
        if query.since_ms < 0 || !length.is_some_and(|ms| (1..=MAX_TRAFFIC_WINDOW_MS).contains(&ms))
        {
            return Err(ApplicationError::InvalidTrafficQuery);
        }
        self.store.get(id).await?;
        let mut report = self.store.traffic(id, query).await?;
        report.counting = self
            .ingress
            .as_ref()
            .is_some_and(|ingress| ingress.counts_traffic());
        Ok(report)
    }

    /// Reads bounded runtime logs for an existing environment.
    /// # Errors
    /// Returns invalid query, absence, storage, or runtime errors.
    pub async fn logs(
        &self,
        id: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
        stream: Option<LogStream>,
    ) -> Result<ApplicationLogs, ApplicationError> {
        if !(1..=1000).contains(&tail)
            || !(1..=86400).contains(&since_seconds)
            || service.is_some_and(|name| name.is_empty() || name.len() > 63)
        {
            return Err(ApplicationError::InvalidLogQuery);
        }
        self.store.get(id).await?;
        Ok(self
            .runtime
            .logs(id, service, tail, since_seconds, stream)
            .await?)
    }
}
