//! Immutable releases: what a tracking environment's successful preparation
//! deployed, reusable by other environments without rebuilding.

use crate::manifest::{
    ApplicationTemplate, GitRevision, RenderContext, RenderTarget, Rendering, SourceRepository,
    ValidatedSource, ValidationErrors,
};
use crate::resource::{ResolvedApplication, ResolvedSource, Sha256Digest};
use crate::{EnvironmentName, ServiceName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Envelope version of [`Release::content_hash`].
const CONTENT_HASH_VERSION: &str = "piqueld-release/v1";

/// One service's rendered build inputs, keyed by field path from the service:
/// every source field `ApplicationSpec::visit_values` lets reference variables
/// (`source.image` or `source.build.*`), plus a Git source's
/// `source.repository` and the `source.commit` it was built from.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuildInputs(BTreeMap<String, String>);

impl BuildInputs {
    /// The inputs of a rendered `source`, built at `commit` when it is a Git source.
    #[must_use]
    pub fn new(source: &ValidatedSource, commit: Option<&str>) -> Self {
        let mut inputs = source.rendered_inputs();
        if let ValidatedSource::Git { repository, .. } = source {
            let url = match repository {
                SourceRepository::Git(repository) => repository.url.clone(),
                SourceRepository::Manifest(_) => "self".into(),
            };
            inputs.insert("source.repository".into(), url);
            if let Some(commit) = commit {
                inputs.insert("source.commit".into(), commit.into());
            }
        }
        Self(inputs)
    }

    /// Each field's rendered value.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// Fields whose values differ from `other`'s, including fields only one has.
    #[must_use]
    pub fn differences(&self, other: &Self) -> Vec<String> {
        crate::changes::changed_keys(&self.0, &other.0)
            .cloned()
            .collect()
    }
}

/// Each service's [`BuildInputs`]: what a release's images were built or
/// pulled from. An environment may run a release's images only where its own
/// rendering of the release's manifest has the same fingerprint.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuildFingerprint(BTreeMap<ServiceName, BuildInputs>);

impl BuildFingerprint {
    /// Each service's build inputs.
    #[must_use]
    pub const fn services(&self) -> &BTreeMap<ServiceName, BuildInputs> {
        &self.0
    }
}

/// The immutable output of one preparation in a tracking environment: its
/// manifest, and each service's source provenance and prepared image. It
/// belongs to the application, and preparations with the same
/// [`Self::content_hash`] share it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The manifest as captured, with references unresolved.
    template: ApplicationTemplate,
    /// Commit the manifest was read from; absent for saved manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// Each service's provenance and image: a registry digest for images
    /// pulled by reference, the commit and local image ID for Git builds.
    sources: BTreeMap<ServiceName, ResolvedSource>,
}

impl Release {
    /// Records what `target`, prepared from `template` read at `commit` when
    /// repository-backed, runs.
    #[must_use]
    pub fn new(
        template: ApplicationTemplate,
        commit: Option<String>,
        target: &ResolvedApplication,
    ) -> Self {
        Self {
            template,
            commit,
            sources: target
                .services
                .iter()
                .map(|service| (service.logical_name.clone(), service.source.clone()))
                .collect(),
        }
    }

    /// The manifest as captured, with references unresolved.
    #[must_use]
    pub const fn template(&self) -> &ApplicationTemplate {
        &self.template
    }

    /// Commit the manifest was read from; absent for saved manifests.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    /// Each service's provenance and prepared image.
    #[must_use]
    pub const fn sources(&self) -> &BTreeMap<ServiceName, ResolvedSource> {
        &self.sources
    }

    /// The build inputs each service's image was prepared from.
    #[must_use]
    pub fn fingerprint(&self) -> BuildFingerprint {
        BuildFingerprint(
            self.sources
                .iter()
                .map(|(service, source)| (service.clone(), source.build_inputs()))
                .collect(),
        )
    }

    /// Versioned SHA-256 over the whole manifest, its commit, and every
    /// service's source and image. The manifest's name and location are
    /// included, since `${{ app.name }}` and `${{ git.branch }}` render them,
    /// so only preparations that render alike share a release.
    ///
    /// # Panics
    ///
    /// Panics only if the release cannot be serialized, a bug in its types.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "Plain structs with string-keyed maps always serialize to JSON."
    )]
    pub fn content_hash(&self) -> Sha256Digest {
        #[derive(Serialize)]
        struct Content<'a> {
            version: &'static str,
            template: &'a ApplicationTemplate,
            commit: Option<&'a str>,
            sources: &'a BTreeMap<ServiceName, ResolvedSource>,
        }
        let bytes = serde_json::to_vec(&Content {
            version: CONTENT_HASH_VERSION,
            template: &self.template,
            commit: self.commit(),
            sources: &self.sources,
        })
        .expect("release fields and string-keyed maps are JSON-compatible");
        Sha256Digest::parse(format!("sha256:{:x}", Sha256::digest(bytes)))
            .expect("a SHA-256 digest is 64 lowercase hexadecimal digits")
    }

    /// Renders the release's own manifest for `environment`, with that
    /// environment's variables, at the release's commit, and pins `"self"`
    /// sources to it. Nothing is rebuilt: compile the result with
    /// [`crate::compile_release`], which requires every build input to
    /// render as it did for the release.
    ///
    /// # Errors
    ///
    /// Returns the rendering's errors, e.g. a variable without a value there.
    pub fn render(
        &self,
        environment: EnvironmentName,
        deployment: Option<String>,
    ) -> Result<Rendering, ValidationErrors> {
        let git = self
            .template
            .spec()
            .manifest
            .as_ref()
            .zip(self.commit.as_ref())
            .map(|(manifest, commit)| GitRevision {
                branch: manifest.repository.branch.clone(),
                sha: commit.clone(),
            });
        let mut rendering = self.template.render(&RenderContext {
            target: RenderTarget::Environment(environment),
            git,
            deployment,
        })?;
        if let Some(commit) = &self.commit {
            rendering.application = rendering.application.pin_manifest_sources(commit);
        }
        Ok(rendering)
    }
}

#[cfg(test)]
mod tests;
