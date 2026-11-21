//! Retention of the images this installation builds: cleanup, and making
//! sure a retained image is present before a deployment uses it again.
use super::{Controller, DockerApi, OperationError};
use piqueld_core::Phase;
use piqueld_core::{
    ImmutableImage, InstanceId, RepositoryDigest, ServiceImage, ServiceName,
    api::{ImageStatus, RegistryImageStatus},
    build_machine::BuildRepository,
    resource::ResolvedSource,
};
use piqueld_docker::{DockerError, LocalImage};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::sync::{Notify, OwnedRwLockReadGuard, RwLock, watch};
use tokio_util::sync::CancellationToken;

/// Cleanup's schedule and state, and the lock that keeps it from removing an
/// image an operation or promotion still uses.
pub(super) struct ImageRetention {
    /// Held shared by each running operation, from start to finish, and by
    /// each promotion and restore, from before it checks images until its
    /// target is saved; exclusively by cleanup while it reads the retention
    /// roots and removes images. Cleanup only tries to take it, so it runs
    /// between operations and delays one by at most a pass. Shared with
    /// the runtime boundary, which accepts promotions.
    in_use: Arc<RwLock<()>>,
    /// Successful deployments per environment whose images are kept.
    keep_deployments: u32,
    /// Successful deployments per environment whose images the build
    /// registry keeps, never fewer than `keep_deployments`.
    registry_keep_deployments: u32,
    /// Time between periodic cleanups.
    interval: Duration,
    /// Wakes cleanup when an operation finishes.
    requested: Notify,
    status: watch::Sender<ImageStatus>,
}

impl Default for ImageRetention {
    fn default() -> Self {
        Self::new(&piqueld_config::ImagesConfig::default())
    }
}

impl ImageRetention {
    pub(super) fn new(config: &piqueld_config::ImagesConfig) -> Self {
        Self {
            in_use: Arc::default(),
            keep_deployments: config.keep_deployments,
            registry_keep_deployments: config
                .registry_keep_deployments
                .max(config.keep_deployments),
            interval: Duration::from_secs(config.cleanup_interval_seconds),
            requested: Notify::new(),
            status: watch::Sender::default(),
        }
    }

    /// Asks for a cleanup soon, e.g. once a deployment finished.
    pub(super) fn request_cleanup(&self) {
        self.requested.notify_one();
    }
}

/// Proof that cleanup isn't running and won't start. Operations hold it until
/// they stop; promotions and restores from before choosing their images until
/// their target is saved, after which those images are retention roots.
pub struct ImagesInUse {
    _guard: OwnedRwLockReadGuard<()>,
}

impl ImagesInUse {
    /// Keeps cleanup, which takes `lock` exclusively, from removing images
    /// until the returned guard drops.
    pub async fn hold(lock: Arc<RwLock<()>>) -> Self {
        Self {
            _guard: lock.read_owned().await,
        }
    }

    /// Makes sure every image of `sources` is present in `docker` before a
    /// deployment uses it again, pulling a missing registry image again by
    /// its digest. Promoting a release and restoring a deployment call it
    /// through the runtime boundary (see `RuntimeBoundary::reuse_images`),
    /// keeping this guard until they saved their target.
    ///
    /// # Errors
    /// `ImageUnavailable`, naming the service and image, for the first
    /// missing image that is a build or that can't be pulled; Docker errors
    /// when images can't be listed.
    pub async fn ensure<'a>(
        &self,
        docker: &impl DockerApi,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> Result<(), OperationError> {
        let present = LocalImage::present(&docker.images().await?);
        for (service, source) in present.missing(sources) {
            let image = ServiceImage {
                service: service.clone(),
                image: source.image(),
            };
            let Some(digest) = source.repository_digest() else {
                return Err(OperationError::ImageUnavailable {
                    image,
                    source: None,
                });
            };
            if let Err(error) = docker.resolve_image(digest.as_str()).await {
                return Err(OperationError::ImageUnavailable {
                    image,
                    source: Some(error),
                });
            }
        }
        Ok(())
    }
}

/// What one cleanup did in the build registry.
struct RegistryPass {
    /// The build registry's repository, `build.repository`.
    repository: BuildRepository,
    /// Pushed images still in the registry.
    kept: u32,
    /// Pushed images it deleted.
    deleted: u32,
    /// Why it stopped before deleting every unused image, if it did.
    failure: Option<String>,
}

impl RegistryPass {
    /// The registry's status after this pass, adding its deletions to those
    /// since the daemon started, as of `earlier`.
    fn status(self, earlier: Option<&RegistryImageStatus>) -> RegistryImageStatus {
        let deleted_before = earlier
            .filter(|earlier| earlier.repository == self.repository.to_string())
            .map_or(0, |earlier| earlier.deleted_images);
        RegistryImageStatus {
            repository: self.repository.to_string(),
            images: self.kept,
            deleted_images: deleted_before + self.deleted,
            failure: self.failure,
        }
    }
}

impl<D: DockerApi> Controller<D> {
    /// Keeps cleanup from removing images until the returned guard drops.
    pub async fn images_in_use(&self) -> ImagesInUse {
        ImagesInUse::hold(Arc::clone(&self.images.in_use)).await
    }

    /// The lock operations and promotions hold against cleanup, for the
    /// runtime boundary.
    pub(super) fn images_lock(&self) -> Arc<RwLock<()>> {
        Arc::clone(&self.images.in_use)
    }

    /// The images this installation built, as of the last cleanup.
    #[must_use]
    pub fn image_status(&self) -> watch::Receiver<ImageStatus> {
        self.images.status.subscribe()
    }

    /// Removes each image this installation built that no retention root
    /// keeps, by ID or repository digest, and no container uses, one by one,
    /// by ID, then health probe configs no service mounts, then cleans up
    /// the build registry (see [`Self::clean_registry`]). Returns
    /// `false` without doing anything while an operation runs or a promotion
    /// may be recording images; the next request or period cleans up
    /// instead.
    ///
    /// # Errors
    /// Returns storage errors, or Docker errors listing images or pruning
    /// probe configs. A failed removal is journaled and the image kept.
    pub async fn clean_images(&self) -> Result<bool, OperationError> {
        let Ok(_exclusive) = self.images.in_use.try_write() else {
            return Ok(false);
        };
        let instance = self.store.instance_id();
        let retained = self
            .store
            .retained_images(self.images.keep_deployments)
            .await?;
        let (mut kept, mut reclaimed) = (0_u32, 0_u64);
        // The registry digests of the images the engine still has, which
        // build reuse may deploy again.
        let mut on_host = BTreeSet::new();
        for image in self.docker.images().await? {
            if !image.built_by(instance) {
                continue;
            }
            if image.used
                || image.retained_by(&retained)
                || !self.remove_image(instance, &image).await?
            {
                kept += 1;
                on_host.extend(image.repo_digests);
            } else {
                reclaimed += image.size;
            }
        }
        // Old probe configs are about as long-lived as old images.
        self.docker.prune_health_probes(instance).await?;
        let registry = match self.docker.build_repository() {
            Some(repository) => Some(self.clean_registry(repository, &on_host).await?),
            None => None,
        };
        let now = piqueld_store::now_ms();
        self.images.status.send_modify(|status| {
            status.images = kept;
            status.reclaimed_bytes += reclaimed;
            status.cleaned_at_ms = Some(now);
            status.registry = registry.map(|pass| pass.status(status.registry.as_ref()));
        });
        Ok(true)
    }

    /// Deletes from the build registry each image this host's
    /// builds pushed to `repository` that no retention root keeps, counting
    /// `registry_keep_deployments` successful deployments, and that the
    /// engine no longer has (`on_host`), one by one, by digest, and forgets
    /// it. Skips the registry, without journaling anything, while it doesn't
    /// answer with success, and stops at the first deletion that fails for a
    /// reason of the whole registry's, such as deletes being disabled, or at
    /// the third in a row that fails for one image's: every other deletion
    /// would fail too, and cleanup holds operations off meanwhile. Fewer
    /// failures of one image's keep that image and go on.
    ///
    /// # Errors
    /// Returns storage errors. A failed deletion is journaled and the image
    /// kept.
    async fn clean_registry(
        &self,
        repository: &BuildRepository,
        on_host: &BTreeSet<RepositoryDigest>,
    ) -> Result<RegistryPass, OperationError> {
        // Failures of one image's own in a row; this many count as the
        // registry's, such as its storage failing, which answers each image's
        // deletion with a 500.
        const IMAGE_FAILURES: u32 = 3;
        let retained = self
            .store
            .retained_images(self.images.registry_keep_deployments)
            .await?;
        let (kept, unused): (Vec<_>, Vec<_>) = self
            .store
            .pushed_images(repository)
            .await?
            .into_iter()
            .partition(|image| {
                on_host.contains(image) || retained.contains(&ImmutableImage::from(image.clone()))
            });
        let count = |images: usize| u32::try_from(images).unwrap_or(u32::MAX);
        let mut pass = RegistryPass {
            repository: repository.clone(),
            kept: count(kept.len()),
            deleted: 0,
            failure: None,
        };
        if unused.is_empty() {
            return Ok(pass);
        }
        if let Err(error) = self.docker.check_registry().await {
            // The chain says why: a refused connection, a name that doesn't
            // resolve or a bad certificate.
            let reason = format!("{:#}", anyhow::Error::new(error));
            tracing::info!(error = %reason, "skipping registry cleanup while the registry is unavailable");
            pass.kept += count(unused.len());
            pass.failure = Some(reason);
            return Ok(pass);
        }
        let mut image_failures = 0;
        for (index, image) in unused.iter().enumerate() {
            let Err(error) = self.delete_pushed_image(image).await? else {
                self.store.forget_pushed_image(image).await?;
                pass.deleted += 1;
                image_failures = 0;
                continue;
            };
            let one_image = matches!(
                &error,
                OperationError::Docker(DockerError::Registry(registry))
                    if !registry.is_registry_wide()
            );
            image_failures = if one_image {
                image_failures + 1
            } else {
                IMAGE_FAILURES
            };
            pass.failure = Some(format!("{:#}", anyhow::Error::new(error)));
            if image_failures == IMAGE_FAILURES {
                pass.kept += count(unused.len() - index);
                break;
            }
            pass.kept += 1;
        }
        Ok(pass)
    }

    /// Cleans up every interval and after operations finish, until
    /// cancelled. Failures are reported as daemon diagnostics.
    pub async fn run_image_cleanup(&self, cancellation: CancellationToken) {
        let mut interval = tokio::time::interval(self.images.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = interval.tick() => {}
                () = self.images.requested.notified() => {}
            }
            if let Err(error) = self.clean_images().await {
                tracing::warn!(%error, "image cleanup failed");
                self.store
                    .report_diagnostic(&error.diagnostic(), None)
                    .await;
            }
        }
    }

    /// Journals removing one image as a daemon action, and returns whether it
    /// was removed. A failed removal is journaled, and the image kept.
    async fn remove_image(
        &self,
        instance: &InstanceId,
        image: &LocalImage,
    ) -> Result<bool, piqueld_store::StoreError> {
        let removed = self
            .journal_image_action(Phase::RemoveImage, image.id.as_str(), async {
                Ok(self.docker.remove_image(instance, &image.id).await?)
            })
            .await?;
        Ok(removed.is_ok())
    }

    /// Journals deleting one pushed image from the build registry
    /// as a daemon action, and returns why it wasn't deleted, if it wasn't.
    /// A failed deletion is journaled, and the image kept.
    async fn delete_pushed_image(
        &self,
        image: &RepositoryDigest,
    ) -> Result<Result<(), OperationError>, piqueld_store::StoreError> {
        self.journal_image_action(Phase::DeleteRegistryImage, image.as_str(), async {
            Ok(self.docker.delete_pushed_image(image).await?)
        })
        .await
    }

    /// Journals `request`, which removes the image `resource` names, as a
    /// daemon action in `phase`, and returns its result.
    async fn journal_image_action(
        &self,
        phase: Phase,
        resource: &str,
        request: impl Future<Output = Result<(), OperationError>>,
    ) -> Result<Result<(), OperationError>, piqueld_store::StoreError> {
        let journal = self
            .store
            .begin_daemon_action(None, phase, Some(resource))
            .await?;
        let removed = self.request_once(&journal, request).await;
        // Unlike `close_action`, only failing to open or close the journal
        // action fails cleanup; a removal that fails, or whose intent can't
        // be recorded, keeps the image.
        self.record_outcome(&journal, &removed).await?;
        Ok(removed)
    }
}
