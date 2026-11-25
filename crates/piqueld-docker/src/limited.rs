//! Process-wide concurrency limits shared by reconciliation and API previews.
//!
//! Applications reconcile concurrently, so per-application limits alone would
//! allow an unbounded number of Docker pulls, builds, and observations. This
//! adapter puts those limits around the shared Docker implementation
//! (including test fakes). Mutations pass through unchanged: the controller
//! serializes them. Cancelling a request drops its permit, allowing the next
//! waiter to proceed.
use super::build_queue::BuildQueue;
use super::{DockerApi, DockerError, DockerTimeout, SwarmState};
use async_trait::async_trait;
use piqueld_core::{
    DesiredNetwork, DesiredService, DesiredVolume, EnvironmentId, ObservedApplication,
};
use std::{collections::BTreeMap, num::NonZeroU8, sync::Arc};
use tokio::sync::{Semaphore, SemaphorePermit};

/// A [`DockerApi`] wrapper that bounds process-wide concurrent Docker work.
pub struct LimitedDocker<D> {
    /// The wrapped implementation that performs the actual requests.
    inner: Arc<D>,
    /// Concurrent image resolutions (pulls).
    images: Semaphore,
    /// Image builds, environments before previews.
    builds: BuildQueue,
    /// Concurrent observations, log reads, and container telemetry's
    /// discovery and usage polls.
    observations: Semaphore,
}
impl<D> LimitedDocker<D> {
    /// Wraps `inner` with 2 image and 8 observation permits, and a queue
    /// running `builds` builds at a time.
    pub fn new(inner: Arc<D>, builds: NonZeroU8) -> Self {
        Self {
            inner,
            images: Semaphore::new(2),
            builds: BuildQueue::new(builds),
            observations: Semaphore::new(8),
        }
    }

    /// Wraps the same implementation in new limits that run `builds` builds
    /// at a time. Permits still held on `self` don't count against the new
    /// limits, so call it before sharing the client.
    #[must_use]
    pub fn with_builds(&self, builds: NonZeroU8) -> Self {
        Self::new(Arc::clone(&self.inner), builds)
    }

    /// Waits for an observation permit, held until the returned guard drops.
    #[expect(
        clippy::expect_used,
        reason = "The observation semaphore is never closed."
    )]
    async fn observing(&self) -> SemaphorePermit<'_> {
        self.observations
            .acquire()
            .await
            .expect("semaphore is never closed")
    }
}

#[async_trait]
impl<D: DockerApi> DockerApi for LimitedDocker<D> {
    async fn application_logs(
        &self,
        instance: &piqueld_core::InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        let _permit = self.observing().await;
        self.inner
            .application_logs(instance, application, service, tail, since, stream)
            .await
    }

    /// Waiting for an observation permit counts against the request budget.
    async fn managed_containers(
        &self,
        instance: &piqueld_core::InstanceId,
    ) -> Result<Vec<super::ManagedContainer>, DockerError> {
        DockerTimeout::Request
            .run("list managed containers", async {
                let _permit = self.observing().await;
                self.inner.managed_containers(instance).await
            })
            .await
    }

    /// Waiting for an observation permit counts against the request budget.
    async fn container_usage(
        &self,
        id: &str,
    ) -> Result<Option<super::ContainerUsage>, DockerError> {
        DockerTimeout::Request
            .run("read container stats", async {
                let _permit = self.observing().await;
                self.inner.container_usage(id).await
            })
            .await
    }

    /// Takes no permit: it lasts as long as the container.
    fn container_logs(&self, id: &str, since: i32) -> super::ContainerLogs {
        self.inner.container_logs(id, since)
    }

    /// Takes no permit: it lasts as long as the daemon.
    fn container_starts(&self, since: i64) -> super::ContainerStarts {
        self.inner.container_starts(since)
    }

    async fn ping(&self) -> Result<(), DockerError> {
        self.inner.ping().await
    }

    async fn create_exec(
        &self,
        instance: &piqueld_core::InstanceId,
        environment: &EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<super::Exec>, DockerError> {
        self.inner.create_exec(instance, environment, request).await
    }

    async fn run_exec(&self, exec: &super::Exec, io: super::ExecIo) -> Result<i64, DockerError> {
        self.inner.run_exec(exec, io).await
    }

    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.ensure_secret(name, value, ownership).await
    }
    async fn remove_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_secrets(names, ownership).await
    }
    async fn remove_unused_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<Vec<String>, DockerError> {
        self.inner.remove_unused_secrets(names, ownership).await
    }
    async fn ensure_swarm(&self, auto: bool) -> Result<SwarmState, DockerError> {
        self.inner.ensure_swarm(auto).await
    }
    /// Waiting for an image permit counts against the resolution budget.
    #[expect(clippy::expect_used, reason = "The image semaphore is never closed.")]
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        DockerTimeout::ImageResolution
            .run("resolve image", async {
                let _permit = self
                    .images
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                self.inner.resolve_image(reference).await
            })
            .await
    }
    async fn build_image(
        &self,
        build: &super::ImageBuild<'_>,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> Result<piqueld_core::BuiltImage, DockerError> {
        let _turn = self.builds.turn(build.priority).await;
        self.inner.build_image(build, log).await
    }
    /// Waiting for an observation permit counts against the request budget.
    async fn observe(&self, id: &EnvironmentId) -> Result<ObservedApplication, DockerError> {
        DockerTimeout::Request
            .run("observe application", async {
                let _permit = self.observing().await;
                self.inner.observe(id).await
            })
            .await
    }
    /// Listing images counts as an observation.
    async fn images(&self) -> Result<Vec<super::LocalImage>, DockerError> {
        let _permit = self.observing().await;
        self.inner.images().await
    }
    async fn remove_image(
        &self,
        instance: &piqueld_core::InstanceId,
        id: &piqueld_core::Sha256Digest,
    ) -> Result<(), DockerError> {
        self.inner.remove_image(instance, id).await
    }
    fn build_repository(&self) -> Option<&piqueld_core::build_machine::BuildRepository> {
        self.inner.build_repository()
    }
    async fn pushed_images(
        &self,
        owner: &piqueld_core::InstanceId,
    ) -> Result<std::collections::BTreeSet<piqueld_core::RepositoryDigest>, DockerError> {
        self.inner.pushed_images(owner).await
    }
    async fn delete_pushed_image(
        &self,
        image: &piqueld_core::RepositoryDigest,
    ) -> Result<(), DockerError> {
        self.inner.delete_pushed_image(image).await
    }
    async fn prune_health_probes(
        &self,
        instance: &piqueld_core::InstanceId,
    ) -> Result<(), DockerError> {
        self.inner.prune_health_probes(instance).await
    }
    async fn prune_files(&self, ownership: &BTreeMap<String, String>) -> Result<(), DockerError> {
        self.inner.prune_files(ownership).await
    }
    async fn ensure_network(&self, value: &DesiredNetwork) -> Result<(), DockerError> {
        self.inner.ensure_network(value).await
    }
    async fn ensure_volume(&self, value: &DesiredVolume) -> Result<(), DockerError> {
        self.inner.ensure_volume(value).await
    }
    async fn ensure_service(&self, value: &DesiredService) -> Result<(), DockerError> {
        self.inner.ensure_service(value).await
    }
    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_service(name, ownership).await
    }
    async fn detach_network(
        &self,
        service: &str,
        network: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.detach_network(service, network, ownership).await
    }
    async fn start_job(&self, job: &piqueld_core::DesiredJobRun) -> Result<(), DockerError> {
        self.inner.start_job(job).await
    }
    async fn job_status(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobStatus, DockerError> {
        let _permit = self.observing().await;
        self.inner.job_status(job).await
    }
    async fn job_output(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobOutput, DockerError> {
        let _permit = self.observing().await;
        self.inner.job_output(job).await
    }
    async fn remove_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: super::JobRuns<'_>,
    ) -> Result<(), DockerError> {
        self.inner.remove_jobs(ownership, runs).await
    }
    async fn wait_jobs_stopped(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: super::JobRuns<'_>,
    ) -> Result<(), DockerError> {
        self.inner.wait_jobs_stopped(ownership, runs).await
    }
    async fn wait_services_stopped(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<(), DockerError> {
        self.inner.wait_services_stopped(job).await
    }
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_network(name, ownership).await
    }
    /// Measuring volumes counts as an observation.
    async fn volumes(
        &self,
        instance: &piqueld_core::InstanceId,
    ) -> Result<Vec<piqueld_core::resource::ManagedVolume>, DockerError> {
        let _permit = self.observing().await;
        self.inner.volumes(instance).await
    }
    async fn remove_volume(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_volume(name, ownership).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn observation_budget_includes_waiting_for_a_permit() {
        use std::error::Error;
        let directory = tempfile::tempdir().expect("the temp dir is created");
        let socket = directory.path().join("docker.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).expect("the listener binds");
        let docker = LimitedDocker::new(
            Arc::new(
                super::super::BollardDocker::connect(&socket).expect("the docker client connects"),
            ),
            NonZeroU8::MIN,
        );
        let permits = docker
            .observations
            .acquire_many(8)
            .await
            .expect("the permits are acquired");
        let started = tokio::time::Instant::now();
        let error = docker
            .observe(&EnvironmentId::parse("app-queued").expect("the environment id parses"))
            .await
            .expect_err("the queued observation times out");
        assert_eq!(started.elapsed(), DockerTimeout::Request.duration());
        assert!(
            error
                .source()
                .expect("the error has a source")
                .is::<tokio::time::error::Elapsed>()
        );
        drop(permits);
        assert_eq!(docker.observations.available_permits(), 8);
    }
}
