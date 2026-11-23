//! The build registry, which the build machine pushes to, through its HTTP
//! API, wherever it runs. The build machine tags each image it pushes for
//! this host `<instance ID>.<UUIDv7>`, so image cleanup finds this
//! installation's images by listing the repository's tags, and deletes those
//! no deployment it keeps there uses, by digest. The registry's garbage
//! collection then frees their layers:
//!
//! ```text
//! GET    https://registry.example.ts.net:5000/v2/piqueld/tags/list?n=1000
//! HEAD   https://registry.example.ts.net:5000/v2/piqueld/manifests/<instance ID>.<UUIDv7>
//! DELETE https://registry.example.ts.net:5000/v2/piqueld/manifests/sha256:<64 hex>
//! ```
use piqueld_core::{InstanceId, RepositoryDigest, Sha256Digest, build_machine::BuildRepository};
use reqwest::{RequestBuilder, Response, StatusCode, header};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// Why the build registry did not delete an image.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The registry could not be reached.
    #[error("the registry at {registry} is unreachable")]
    Unreachable {
        /// The registry's `host[:port]`.
        registry: String,
        /// Why the request failed.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The registry answered a listing of the repository's images with an
    /// error, such as a proxy's 502 while the registry is stopped, or a
    /// request for credentials, which piqueld has none of.
    #[error("the registry at {registry} answered {status}{message}")]
    Unavailable {
        /// The registry's `host[:port]`.
        registry: String,
        /// Its response's status.
        status: StatusCode,
        /// The first error it gave, after a colon, if any.
        message: String,
    },
    /// The registry answered, but refused, such as when deletes are
    /// disabled.
    #[error("the registry refused to delete {image}: {status}{message}")]
    Refused {
        /// The image it kept.
        image: RepositoryDigest,
        /// Its response's status.
        status: StatusCode,
        /// The first error it gave, after a colon, if any.
        message: String,
    },
    /// The image is not in the build registry's repository,
    /// `build.repository`.
    #[error("{0} is not in build.repository")]
    NotPushed(RepositoryDigest),
    /// The registry answered a tag's lookup without a valid digest.
    #[error("the registry gave no digest for tag {0}")]
    NoDigest(String),
    /// The registry answered a listing of the repository's tags with
    /// something that isn't one.
    #[error("the registry at {registry} answered a listing of tags that isn't one")]
    InvalidListing {
        /// The registry's `host[:port]`.
        registry: String,
        /// Why it couldn't be read.
        #[source]
        source: reqwest::Error,
    },
}

impl RegistryError {
    /// Whether the error is the registry's rather than one image's: it
    /// doesn't answer, refuses this client (401, 403), doesn't allow deletes
    /// (405), asks to slow down (429), or a proxy in front of it fails (502,
    /// 503, 504). Deleting any other image would fail the same way.
    #[must_use]
    pub fn is_registry_wide(&self) -> bool {
        match self {
            Self::Unreachable { .. }
            | Self::Unavailable { .. }
            | Self::NoDigest(_)
            | Self::InvalidListing { .. } => true,
            Self::Refused { status, .. } => {
                matches!(status.as_u16(), 401 | 403 | 405 | 429 | 502..=504)
            }
            Self::NotPushed(_) => false,
        }
    }
}

/// The build registry's repository, `build.repository`, and a client of the
/// registry.
#[derive(Debug)]
pub struct BuildRegistry {
    /// The repository, `registry-host[:port]/path`.
    pub(crate) repository: BuildRepository,
    /// The HTTP client, with the timeouts below.
    client: reqwest::Client,
    /// The digest each of this installation's tags names, or none once its
    /// image is gone. A tag never moves, so each is looked up once.
    digests: parking_lot::Mutex<BTreeMap<String, Option<Sha256Digest>>>,
}

impl BuildRegistry {
    /// Longest a connection to the registry may take before it counts as
    /// unreachable, as for the build machine.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Longest a whole request may take.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    /// Tags each page of a listing asks for.
    const PAGE: usize = 1000;
    /// The manifest types a tag's lookup accepts, so the registry answers
    /// with the digest the image was pushed under.
    const MANIFEST_TYPES: &str = "application/vnd.oci.image.manifest.v1+json, \
        application/vnd.docker.distribution.manifest.v2+json, \
        application/vnd.oci.image.index.v1+json, \
        application/vnd.docker.distribution.manifest.list.v2+json";

    /// A client of `repository`'s registry, which need not be reachable.
    ///
    /// # Errors
    /// Returns why the HTTP client could not be created.
    pub(crate) fn new(repository: BuildRepository) -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .timeout(Self::REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            repository,
            client,
            digests: parking_lot::Mutex::default(),
        })
    }

    /// A new tag for an image `owner` pushes, `<owner>.<UUIDv7>`: unique, so
    /// a push never moves another image's tag, and naming the installation,
    /// so [`Self::pushed_images`] finds only its own.
    #[must_use]
    pub fn new_tag(owner: &InstanceId) -> String {
        format!("{owner}.{}", uuid::Uuid::now_v7().simple())
    }

    /// The images `owner`'s builds pushed to the repository that are still
    /// there: lists the repository's tags, page by page, keeps `owner`'s, and
    /// looks each up for its digest, once. A repository the registry doesn't
    /// have yet holds none. Listing it is how image cleanup checks the
    /// registry answers before deleting anything.
    ///
    /// # Errors
    /// Returns why the registry couldn't be listed.
    pub(crate) async fn pushed_images(
        &self,
        owner: &InstanceId,
    ) -> Result<BTreeSet<RepositoryDigest>, RegistryError> {
        let prefix = format!("{owner}.");
        let tags: BTreeSet<String> = self
            .tags()
            .await?
            .into_iter()
            .filter(|tag| tag.starts_with(&prefix))
            .collect();
        let mut images = BTreeSet::new();
        for tag in &tags {
            let known = self.digests.lock().get(tag).cloned();
            let digest = if let Some(digest) = known {
                digest
            } else {
                let digest = self.lookup(tag).await?;
                if let Some(digest) = &digest {
                    self.digests
                        .lock()
                        .insert(tag.clone(), Some(digest.clone()));
                }
                digest
            };
            images.extend(digest.map(|digest| self.repository.image(&digest)));
        }
        self.digests.lock().retain(|tag, _| tags.contains(tag));
        Ok(images)
    }

    /// Every tag of the repository, following the listing's pages until the
    /// last one, or one already read.
    async fn tags(&self) -> Result<Vec<String>, RegistryError> {
        /// A page of the listing.
        #[derive(serde::Deserialize)]
        struct Page {
            /// Its tags; null when the repository has none.
            tags: Option<Vec<String>>,
        }
        let mut tags = Vec::new();
        let mut read = BTreeSet::new();
        let mut next = Some(format!(
            "{}/v2/{}/tags/list?n={}",
            self.base(),
            self.repository.path(),
            Self::PAGE
        ));
        while let Some(url) = next.take()
            && read.insert(url.clone())
        {
            let response = self.send(self.client.get(url)).await?;
            if response.status() == StatusCode::NOT_FOUND {
                let status = response.status();
                let answer = RegistryAnswer::read(response).await;
                if answer.code == "NAME_UNKNOWN" {
                    break;
                }
                return Err(RegistryError::Unavailable {
                    registry: self.repository.registry().to_owned(),
                    status,
                    message: answer.message(),
                });
            }
            next = self.next_page(&response);
            let page =
                response
                    .json::<Page>()
                    .await
                    .map_err(|source| RegistryError::InvalidListing {
                        registry: self.repository.registry().to_owned(),
                        source,
                    })?;
            tags.extend(page.tags.into_iter().flatten());
        }
        Ok(tags)
    }

    /// The digest `tag` names, or none once the registry no longer has it.
    async fn lookup(&self, tag: &str) -> Result<Option<Sha256Digest>, RegistryError> {
        let request = self
            .client
            .head(format!(
                "{}/v2/{}/manifests/{tag}",
                self.base(),
                self.repository.path()
            ))
            .header(header::ACCEPT, Self::MANIFEST_TYPES);
        let response = self.send(request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|digest| digest.to_str().ok())
            .and_then(|digest| Sha256Digest::parse(digest).ok())
            .map(Some)
            .ok_or_else(|| RegistryError::NoDigest(tag.to_owned()))
    }

    /// Sends `request`: its response when it succeeds or answers 404, which
    /// the caller tells apart, and why otherwise.
    async fn send(&self, request: RequestBuilder) -> Result<Response, RegistryError> {
        let response = request
            .send()
            .await
            .map_err(|source| self.unreachable(source))?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(response);
        }
        Err(RegistryError::Unavailable {
            registry: self.repository.registry().to_owned(),
            status,
            message: RegistryAnswer::read(response).await.message(),
        })
    }

    /// The next page of a listing: the target of its `Link` header's value
    /// whose relation is `next`, resolved against the page's own URL, then
    /// sent to the registry's own scheme and host, in case a proxy in front
    /// of it names another. None on the last page.
    ///
    /// ```text
    /// Link: </v2/piqueld/tags/list?last=b&n=1000>; rel="next"
    ///   -> https://registry.example.ts.net:5000/v2/piqueld/tags/list?last=b&n=1000
    /// ```
    fn next_page(&self, response: &Response) -> Option<String> {
        let target = response
            .headers()
            .get_all(header::LINK)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .flat_map(|header| header.split(','))
            .find_map(|value| {
                let (target, parameters) = value.trim().strip_prefix('<')?.split_once('>')?;
                parameters
                    .split(';')
                    .filter_map(|parameter| parameter.trim().strip_prefix("rel="))
                    .any(|relations| {
                        relations
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|relation| relation.eq_ignore_ascii_case("next"))
                    })
                    .then_some(target)
            })?;
        let resolved = response.url().join(target).ok()?;
        let query = resolved
            .query()
            .map_or_else(String::new, |query| format!("?{query}"));
        Some(format!("{}{}{query}", self.base(), resolved.path()))
    }

    /// Deletes `image` from the registry by digest, which also removes its
    /// tags. An image the registry says it doesn't have counts as deleted;
    /// any other 404, such as a proxy's, doesn't.
    ///
    /// # Errors
    /// Returns why the registry did not delete it.
    pub(crate) async fn delete(&self, image: &RepositoryDigest) -> Result<(), RegistryError> {
        let digest = self
            .repository
            .digest_of(image)
            .ok_or_else(|| RegistryError::NotPushed(image.clone()))?;
        let response = self
            .client
            .delete(format!(
                "{}/v2/{}/manifests/{digest}",
                self.base(),
                self.repository.path()
            ))
            .send()
            .await
            .map_err(|source| self.unreachable(source))?;
        let status = response.status();
        if !status.is_success() {
            let answer = RegistryAnswer::read(response).await;
            if !(status == StatusCode::NOT_FOUND && answer.unknown()) {
                return Err(RegistryError::Refused {
                    image: image.clone(),
                    status,
                    message: answer.message(),
                });
            }
        }
        for known in self.digests.lock().values_mut() {
            if known.as_ref() == Some(&digest) {
                *known = None;
            }
        }
        Ok(())
    }

    /// The registry's URL: HTTPS, which Docker requires of a registry on
    /// another machine, or HTTP for one on this host's loopback, which
    /// Docker allows without it.
    fn base(&self) -> String {
        let registry = self.repository.registry();
        let host = registry.rsplit_once(':').map_or(registry, |(host, _)| host);
        let loopback = host == "localhost"
            || host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|address| address.is_loopback());
        let scheme = if loopback { "http" } else { "https" };
        format!("{scheme}://{registry}")
    }

    /// [`RegistryError::Unreachable`], for why a request got no answer.
    fn unreachable(&self, source: reqwest::Error) -> RegistryError {
        RegistryError::Unreachable {
            registry: self.repository.registry().to_owned(),
            source: source.into(),
        }
    }
}

/// The first error of a registry's error response, by the distribution
/// specification: `{"errors":[{"code":"UNSUPPORTED","message":"…"}]}`.
#[derive(Default, serde::Deserialize)]
struct RegistryAnswer {
    /// Its code, such as `MANIFEST_UNKNOWN`.
    #[serde(default)]
    code: String,
    /// Its message, for people.
    #[serde(default)]
    message: String,
}

impl RegistryAnswer {
    /// Reads `response`'s first error; an empty one if it has none.
    async fn read(response: Response) -> Self {
        /// The response's body: its errors.
        #[derive(serde::Deserialize)]
        struct Body {
            /// The errors, the first of which matters.
            errors: Vec<RegistryAnswer>,
        }
        response
            .json::<Body>()
            .await
            .ok()
            .and_then(|body| body.errors.into_iter().next())
            .unwrap_or_default()
    }

    /// Whether the registry doesn't have the manifest or the repository.
    fn unknown(&self) -> bool {
        matches!(self.code.as_str(), "MANIFEST_UNKNOWN" | "NAME_UNKNOWN")
    }

    /// The message, after a colon, if any.
    fn message(&self) -> String {
        if self.message.is_empty() {
            String::new()
        } else {
            format!(": {}", self.message)
        }
    }
}

#[cfg(test)]
mod tests;
