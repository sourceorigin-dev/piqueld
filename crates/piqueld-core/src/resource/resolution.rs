//! Immutable resolutions of service sources and secrets, supplied to
//! compilation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::CompileError;
use super::application::SourceOrigin;
#[cfg(doc)]
use super::compile_application;
use super::image_repository;
use crate::manifest::domain::ValidatedSecretMount;
use crate::manifest::{
    FileSource, GitRepository, NormalizedApplication, SourceRepository, ValidatedBuild,
    ValidatedSource,
};
use crate::{
    ApiErrorCode, BuildInputs, BuiltImage, GitCommit, ImageReference, ImmutableImage,
    RepositoryDigest, ServiceName,
};

/// Immutable image resolution used by the Docker runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedSource {
    /// A requested image resolved to an immutable repository digest.
    Image {
        /// The image reference requested by the user.
        requested: ImageReference,
        /// The immutable image reference used at runtime.
        digest_reference: RepositoryDigest,
    },
    /// A Git revision built into an immutable image.
    Git {
        /// Exact source and build inputs requested.
        #[schema(value_type = ValidatedSource)]
        requested: GitSource,
        /// Resolved full commit hash.
        commit: GitCommit,
        /// The image it was built into: Docker's content-addressed local
        /// image ID, or its digest in the build registry.
        image_id: BuiltImage,
    },
}

/// A Git source whose repository is resolved, as a build checks it out:
/// `"self"` already replaced by the manifest's repository. It reads and writes
/// as the [`ValidatedSource`] it is, tagged `"type": "git"`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(into = "ValidatedSource", try_from = "ValidatedSource")]
pub struct GitSource {
    /// Repository and revision to check out.
    pub repository: GitRepository,
    /// Build instructions.
    pub build: ValidatedBuild,
}

/// A source that isn't a [`GitSource`]: an image, or a Git source whose
/// repository is still `"self"`.
#[derive(Debug, thiserror::Error)]
#[error("a built source must be a Git source of a resolved repository")]
pub struct NotGitSource;

impl From<GitSource> for ValidatedSource {
    fn from(source: GitSource) -> Self {
        Self::Git {
            repository: SourceRepository::Git(source.repository),
            build: source.build,
        }
    }
}

impl TryFrom<ValidatedSource> for GitSource {
    type Error = NotGitSource;

    fn try_from(source: ValidatedSource) -> Result<Self, NotGitSource> {
        match source {
            ValidatedSource::Git {
                repository: SourceRepository::Git(repository),
                build,
            } => Ok(Self { repository, build }),
            ValidatedSource::Git { .. } | ValidatedSource::Image { .. } => Err(NotGitSource),
        }
    }
}

/// Invalid raw image-resolution result received at a runtime boundary.
#[derive(Debug, thiserror::Error)]
pub enum ResolvedImageError {
    /// Requested image syntax is invalid.
    #[error("invalid requested image reference")]
    Reference(#[from] crate::images::ImageReferenceError),
    /// The runtime did not return a repository digest.
    #[error("invalid resolved repository digest")]
    Digest(#[from] crate::images::RepositoryDigestError),
}

impl ResolvedSource {
    /// Checks raw image-resolution input and the runtime's repository digest.
    ///
    /// # Errors
    /// Returns the invalid reference or digest cause.
    pub fn parse_image(
        requested: impl Into<String>,
        digest: impl Into<String>,
    ) -> Result<Self, ResolvedImageError> {
        Ok(Self::Image {
            requested: ImageReference::parse(requested)?,
            digest_reference: RepositoryDigest::parse(digest)?,
        })
    }

    /// Returns the checked immutable image used by Docker.
    #[must_use]
    pub fn image(&self) -> ImmutableImage {
        match self {
            Self::Image {
                digest_reference, ..
            } => digest_reference.clone().into(),
            Self::Git { image_id, .. } => image_id.clone().into(),
        }
    }

    /// Returns the source the user requested before resolution.
    #[must_use]
    pub fn requested(&self) -> ValidatedSource {
        match self {
            Self::Image { requested, .. } => ValidatedSource::Image {
                image: requested.clone(),
            },
            Self::Git { requested, .. } => requested.clone().into(),
        }
    }

    /// The commit a Git build was resolved to.
    #[must_use]
    pub fn commit(&self) -> Option<&GitCommit> {
        match self {
            Self::Image { .. } => None,
            Self::Git { commit, .. } => Some(commit),
        }
    }

    /// The build inputs this source was prepared from.
    #[must_use]
    pub fn build_inputs(&self) -> BuildInputs {
        BuildInputs::new(&self.requested(), self.commit())
    }

    /// Whether this is a valid immutable resolution of `source`.
    ///
    /// Images must match the requested reference exactly and resolve within the
    /// same repository. Git sources must be identical and honour a pinned
    /// commit when one is set.
    pub(super) fn resolves(&self, source: &ValidatedSource) -> bool {
        match (source, self) {
            (
                ValidatedSource::Image { image },
                Self::Image {
                    requested,
                    digest_reference,
                },
            ) => {
                image == requested
                    && image_repository(image.as_str()).is_some_and(|repository| {
                        image_repository(digest_reference.as_str()) == Some(repository)
                    })
            }
            (
                ValidatedSource::Git {
                    repository: SourceRepository::Git(repository),
                    build,
                },
                Self::Git {
                    requested, commit, ..
                },
            ) => {
                requested.repository == *repository
                    && requested.build == *build
                    && repository
                        .commit
                        .as_ref()
                        .is_none_or(|pinned| pinned == commit.as_str())
            }
            _ => false,
        }
    }

    /// Returns the immutable image reference used by Docker.
    #[must_use]
    pub fn digest_reference(&self) -> &str {
        match self {
            Self::Image {
                digest_reference, ..
            } => digest_reference.as_str(),
            Self::Git { image_id, .. } => image_id.as_str(),
        }
    }
}

/// A Docker secret name and its file destination, with no secret value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretFile {
    /// Immutable version name in Docker.
    pub secret_name: String,
    /// Container file destination.
    pub target: String,
}

/// Immutable resolutions supplied to application compilation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    /// Resolved service sources keyed by logical service name.
    pub sources: BTreeMap<ServiceName, ResolvedSource>,
    /// Logical secrets pinned to immutable Docker names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_names: BTreeMap<String, String>,
}

impl ResolutionSet {
    /// The pinned Docker secret of each mount in `secrets`. Must only run after
    /// [`Self::errors`] finds none: it panics on an unpinned secret.
    pub(super) fn secret_files<'a>(
        &'a self,
        secrets: &'a [ValidatedSecretMount],
    ) -> impl Iterator<Item = SecretFile> + 'a {
        secrets.iter().map(|secret| SecretFile {
            secret_name: self.secret_names[&secret.name].clone(),
            target: secret.target.clone(),
        })
    }

    /// Collects every reason [`compile_application`] cannot proceed with these
    /// resolutions: unresolved sources, unpinned secrets, uncaptured
    /// repository files, and resolutions that do not match their source. A
    /// release's sources are first checked field by field against the build
    /// inputs they were prepared from, so a mismatch names what differs.
    pub(super) fn errors(
        &self,
        app: &NormalizedApplication,
        origin: SourceOrigin,
    ) -> Vec<CompileError> {
        let mut errors = preview_resolution(app, self)
            .into_iter()
            .map(|requirement| match requirement {
                ResolutionRequirement::ResolveImage { service, .. }
                | ResolutionRequirement::BuildGit { service, .. } => CompileError::new(
                    ApiErrorCode::SourceUnresolved,
                    service.as_str(),
                    "service source has not been resolved to an immutable image",
                ),
            })
            .collect::<Vec<_>>();
        for service in &app.spec().services {
            for file in &service.files {
                if let FileSource::Path(path) = &file.source
                    && app.files().get(path).is_none()
                {
                    errors.push(CompileError::new(
                        ApiErrorCode::FileUnresolved,
                        service.name.as_str(),
                        format!("repository file {path} was not captured with the manifest"),
                    ));
                }
            }
        }
        let mounts = app
            .spec()
            .services
            .iter()
            .map(|service| (service.name.as_str(), &service.secrets))
            .chain(
                app.spec()
                    .jobs
                    .iter()
                    .map(|job| (job.name.as_str(), &job.secrets)),
            );
        for (owner, secrets) in mounts {
            for secret in secrets {
                if !self.secret_names.contains_key(&secret.name) {
                    errors.push(CompileError::new(
                        ApiErrorCode::SecretUnresolved,
                        owner,
                        format!("secret {} has not been pinned", secret.name),
                    ));
                }
            }
        }
        for service in &app.spec().services {
            let Some(resolved) = self.sources.get(&service.name) else {
                continue;
            };
            let incompatible = match origin {
                SourceOrigin::Prepared => Vec::new(),
                SourceOrigin::Release => resolved
                    .build_inputs()
                    .differences(&BuildInputs::new(&service.source, resolved.commit())),
            };
            if !incompatible.is_empty() {
                errors.extend(incompatible.into_iter().map(|field| {
                    CompileError::new(
                        ApiErrorCode::ReleaseIncompatible,
                        format!("{}.{field}", service.name),
                        "renders differently in this environment than in the release, whose images were built from it",
                    )
                }));
            } else if !resolved.resolves(&service.source) {
                errors.push(CompileError::new(
                    ApiErrorCode::SourceResolutionMismatch,
                    service.name.as_str(),
                    "resolved source does not immutably resolve the normalized service source",
                ));
            }
        }
        errors
    }
}

/// Resolution work still required before compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionRequirement {
    /// Resolve an image source to an immutable digest.
    ResolveImage {
        /// Logical service requesting resolution.
        service: ServiceName,
        /// Requested image reference.
        reference: String,
    },
    /// Resolve a Git revision and build its image.
    BuildGit {
        /// Logical service requesting a build.
        service: ServiceName,
        /// Explicit Git source configuration.
        source: ValidatedSource,
    },
}

/// Returns the image resolutions still needed before compilation.
///
/// Every service without an entry in `resolutions.sources` yields one
/// requirement: image sources need digest resolution, Git sources need a build.
/// Existing entries are not checked here; `compile_application` rejects
/// mismatched ones.
#[must_use]
pub fn preview_resolution(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<ResolutionRequirement> {
    app.spec()
        .services
        .iter()
        .filter(|service| !resolutions.sources.contains_key(&service.name))
        .map(|service| match &service.source {
            ValidatedSource::Image { image } => ResolutionRequirement::ResolveImage {
                service: service.name.clone(),
                reference: image.to_string(),
            },
            ValidatedSource::Git { .. } => ResolutionRequirement::BuildGit {
                service: service.name.clone(),
                source: service.source.clone(),
            },
        })
        .collect()
}
