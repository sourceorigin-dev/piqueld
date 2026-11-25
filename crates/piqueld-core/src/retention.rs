//! Retention: which deployments keep their images when cleanup runs, whether
//! a release's images are still present, and the configuration file's
//! `[retention]`, `[build_history]` and `[images]` tables, which the daemon's
//! store prunes its history by.

use crate::{
    ApiErrorCode, BuiltImage, DiagnosticSeverity, ImmutableImage, PlanDiagnostic, RepositoryDigest,
    ServiceName, resource::ResolvedApplication, resource::ResolvedSource,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Why cleanup keeps a deployment's images. Cleanup removes only images this
/// installation built that no root keeps.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RetentionRoot {
    /// What an environment or preview currently runs.
    Current,
    /// The latest deployment of an environment or preview: in progress, or
    /// finished and retried with its prepared target until a newer one
    /// replaces it.
    Latest,
    /// One of an environment's last `keep_deployments` successful
    /// deployments, which restoring a deployment deploys again.
    /// Previews keep none.
    Recent,
    /// What an environment that a promoted environment promotes from
    /// currently runs: the release `env promote` takes by default. Promoting
    /// then saves its target, which `Latest` keeps, while holding cleanup off.
    PromotionSource,
}

impl RetentionRoot {
    /// Every root, in the order cleanup reads them.
    pub const ALL: [Self; 4] = [
        Self::Current,
        Self::Latest,
        Self::Recent,
        Self::PromotionSource,
    ];
}

/// A service and the image it runs.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ServiceImage {
    /// Logical service name.
    pub service: ServiceName,
    /// Its immutable image.
    pub image: ImmutableImage,
}

/// Whether a release's images are still present, so it can be deployed again.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReleaseAvailability {
    /// Every image is present.
    Present,
    /// Only registry images are missing: deploying the release pulls them
    /// again by digest.
    Pullable {
        /// The missing images.
        missing: Vec<ServiceImage>,
    },
    /// Built images are missing, so the release can't be deployed again.
    Unavailable {
        /// The missing images, built or pulled.
        missing: Vec<ServiceImage>,
    },
}

impl ReleaseAvailability {
    /// One blocking `image_unavailable` diagnostic per missing image of an
    /// unavailable release, since deploying it fails; none otherwise.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<PlanDiagnostic> {
        match self {
            Self::Present | Self::Pullable { .. } => Vec::new(),
            Self::Unavailable { missing } => missing
                .iter()
                .map(|missing| {
                    PlanDiagnostic::new(
                        ApiErrorCode::ImageUnavailable,
                        DiagnosticSeverity::Error,
                        missing.service.as_str(),
                        format!(
                            "the release can't be deployed again: its built images were removed (image {} of service {} is missing)",
                            missing.image, missing.service
                        ),
                    )
                })
                .collect(),
        }
    }
}

/// The images present in the local Docker Engine, by local image ID and by
/// repository digest.
#[derive(Clone, Debug, Default)]
pub struct LocalImages(BTreeSet<ImmutableImage>);

impl LocalImages {
    /// Images present under each of these IDs and repository digests.
    pub fn new(images: impl IntoIterator<Item = ImmutableImage>) -> Self {
        Self(images.into_iter().collect())
    }

    /// Whether `image` is present.
    #[must_use]
    pub fn contains(&self, image: &ImmutableImage) -> bool {
        self.0.contains(image)
    }

    /// The services of `sources` whose images aren't present, in order.
    pub fn missing<'a>(
        &self,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> Vec<(&'a ServiceName, &'a ResolvedSource)> {
        sources
            .into_iter()
            .filter(|(_, source)| !self.contains(&source.image()))
            .collect()
    }

    /// Whether each image of `sources` is present, and if not, whether the
    /// missing ones can be pulled again.
    pub fn availability<'a>(
        &self,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> ReleaseAvailability {
        let missing = self.missing(sources);
        let pullable = missing
            .iter()
            .all(|(_, source)| source.repository_digest().is_some());
        let missing = missing
            .into_iter()
            .map(|(service, source)| ServiceImage {
                service: service.clone(),
                image: source.image(),
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            ReleaseAvailability::Present
        } else if pullable {
            ReleaseAvailability::Pullable { missing }
        } else {
            ReleaseAvailability::Unavailable { missing }
        }
    }
}

impl ResolvedSource {
    /// The registry digest Docker can pull again if the image is missing.
    /// An image built on the local engine can't be recovered: rebuilding
    /// may produce another one.
    #[must_use]
    pub const fn repository_digest(&self) -> Option<&RepositoryDigest> {
        match self {
            Self::Image {
                digest_reference, ..
            }
            | Self::Git {
                image_id: BuiltImage::Pushed(digest_reference),
                ..
            } => Some(digest_reference),
            Self::Git {
                image_id: BuiltImage::Local(_),
                ..
            } => None,
        }
    }
}

impl ResolvedApplication {
    /// Each service and its resolved source. Jobs run their service's image.
    pub fn sources(&self) -> impl Iterator<Item = (&ServiceName, &ResolvedSource)> {
        self.services
            .iter()
            .map(|service| (&service.logical_name, &service.source))
    }

    /// Every image its services and jobs run.
    pub fn images(&self) -> impl Iterator<Item = ImmutableImage> {
        self.services
            .iter()
            .chain(self.jobs.iter().map(|job| &job.container))
            .map(|service| service.image.clone())
    }
}

/// How long history is kept. `0` keeps it forever.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Days finished operations are kept; `0` keeps them forever. Older ones
    /// are removed during reconciliation, except each environment's latest
    /// operation and those in its deployment history or with job cleanup
    /// pending.
    pub finished_operation_days: u64,
    /// Days each environment's deployment history is kept; `0` keeps it
    /// forever. A deployment is removed only once it is older than this and
    /// beyond the newest `deployments_per_environment`, and never while it is
    /// current or latest, among the newest `images.keep_deployments`
    /// successful ones (at least one), restored by a remaining deployment, or
    /// has jobs to clean up. Of a preview's deployments that finished over an
    /// hour ago, only its newest successful one and those a remaining one
    /// restored stay. Unused releases beyond each application's newest 50, and
    /// the build and job records of removed deployments, go too. `0` here or
    /// in `deployments_per_environment` stops all pruning.
    pub deployment_days: u64,
    /// Deployments each environment keeps however old they are; `0` keeps
    /// every deployment, release and build record, as for `deployment_days`.
    pub deployments_per_environment: u32,
    /// Days application events are kept; `0` keeps them forever. Deleting an
    /// application also removes its events.
    pub event_days: u64,
    /// Days events not tied to an application, such as those about shared
    /// infrastructure, are kept; `0` keeps them forever.
    pub daemon_event_days: u64,
    /// Days the audit trail of API requests is kept; `0` keeps it forever.
    pub audit_days: u64,
    /// Days each environment's traffic counts are kept; `0` keeps them
    /// forever. Deleting an environment also removes its counts.
    pub traffic_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            finished_operation_days: 10,
            deployment_days: 90,
            deployments_per_environment: 50,
            event_days: 90,
            daemon_event_days: 90,
            audit_days: 365,
            traffic_days: 90,
        }
    }
}

/// How much output of each build is kept, and for how long. Builds
/// themselves are listed until deployment history pruning removes their
/// operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BuildHistoryConfig {
    /// The most output kept per build, in bytes. Output within it is kept
    /// whole. Of longer output, a build keeps the first half and the end,
    /// with a note of how many bytes it left out between them when the note
    /// fits.
    #[schema(minimum = 1, maximum = 67_108_864)]
    pub log_max_bytes: u32,
    /// Days a build's output is kept after it finishes. The build stays
    /// listed, marked as expired.
    #[schema(minimum = 1, maximum = 3650)]
    pub log_retention_days: u32,
}
impl Default for BuildHistoryConfig {
    fn default() -> Self {
        Self {
            log_max_bytes: 4 * 1024 * 1024,
            log_retention_days: 30,
        }
    }
}

/// Cleanup of the images this installation builds. Cleanup keeps what every
/// environment and preview runs or is deploying, and removes the rest. It
/// never removes pulled images, images built by hand, or images other
/// installations built on the same Docker Engine. With a build machine, it
/// also deletes from the build registry the images this host's builds pushed
/// that no deployment it keeps there uses.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesConfig {
    /// How many of each environment's latest successful deployments also
    /// keep their images, so their releases can be deployed again.
    pub keep_deployments: u32,
    /// How many of each environment's latest successful deployments keep
    /// their images in the build registry, `build.repository`, and never
    /// fewer than `keep_deployments`. Cleanup deletes the other images this
    /// host's builds pushed there, and skips the registry while it can't be
    /// reached. Without a build machine, it does nothing.
    pub registry_keep_deployments: u32,
    /// Seconds between cleanups. Cleanup also runs after each operation.
    #[schema(minimum = 1, maximum = 86_400)]
    pub cleanup_interval_seconds: u64,
}
impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            keep_deployments: 3,
            registry_keep_deployments: 10,
            cleanup_interval_seconds: 3600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Sha256Digest, manifest::ValidatedSource};
    use std::collections::BTreeMap;

    fn pulled(digest: char) -> ResolvedSource {
        ResolvedSource::parse_image(
            "example.com/web:1",
            format!("example.com/web@sha256:{}", digest.to_string().repeat(64)),
        )
        .unwrap()
    }

    fn built(id: char) -> ResolvedSource {
        ResolvedSource::Git {
            requested: ValidatedSource::Image {
                image: "unused".into(),
            },
            commit: "a".repeat(40),
            image_id: Sha256Digest::parse(format!("sha256:{}", id.to_string().repeat(64)))
                .unwrap()
                .into(),
        }
    }

    #[test]
    fn missing_registry_images_are_pullable_and_missing_builds_unavailable() {
        let (web, api) = (
            ServiceName::parse("web").unwrap(),
            ServiceName::parse("api").unwrap(),
        );
        let sources = BTreeMap::from([(web.clone(), pulled('a')), (api.clone(), built('b'))]);
        let all = LocalImages::new(sources.values().map(ResolvedSource::image));
        assert_eq!(all.availability(&sources), ReleaseAvailability::Present);

        let built_only = LocalImages::new([built('b').image()]);
        assert_eq!(
            built_only.availability(&sources),
            ReleaseAvailability::Pullable {
                missing: vec![ServiceImage {
                    service: web.clone(),
                    image: pulled('a').image()
                }]
            }
        );

        let ReleaseAvailability::Unavailable { missing } =
            LocalImages::default().availability(&sources)
        else {
            panic!("a missing build makes the release unavailable");
        };
        assert_eq!(
            missing.iter().map(|m| &m.service).collect::<Vec<_>>(),
            [&api, &web]
        );

        let mut pushed = built('b');
        if let ResolvedSource::Git { image_id, .. } = &mut pushed {
            *image_id = pulled('c').repository_digest().unwrap().clone().into();
        }
        assert!(matches!(
            LocalImages::default().availability([(&api, &pushed)]),
            ReleaseAvailability::Pullable { .. }
        ));
    }

    /// A plan of a release that can't be deployed again is blocked, like
    /// the promotion; one whose images can be pulled again is not.
    #[test]
    fn only_unavailable_releases_block_plans() {
        let web = ServiceName::parse("web").unwrap();
        let sources = BTreeMap::from([(web, built('b'))]);
        let diagnostics = LocalImages::default().availability(&sources).diagnostics();
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.resource.as_str()))
                .collect::<Vec<_>>(),
            [(ApiErrorCode::ImageUnavailable.as_str(), "web")]
        );
        assert!(diagnostics[0].blocking);
        let pullable = ReleaseAvailability::Pullable {
            missing: Vec::new(),
        };
        assert_eq!(pullable.diagnostics(), []);
        assert_eq!(ReleaseAvailability::Present.diagnostics(), []);
    }
}
