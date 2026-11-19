//! The build registry, which the build machine pushes to, through its HTTP
//! API, wherever it runs. Image
//! cleanup deletes the images this host's builds pushed there, by digest,
//! once no deployment it keeps there uses them, and the registry's garbage
//! collection then frees their layers:
//!
//! ```text
//! GET    https://registry.example.ts.net:5000/v2/
//! DELETE https://registry.example.ts.net:5000/v2/piqueld/manifests/sha256:<64 hex>
//! ```
use piqueld_core::{RepositoryDigest, build_machine::BuildRepository};
use reqwest::StatusCode;
use std::time::Duration;

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
    /// The registry answered its API's base endpoint with an error, such as
    /// a proxy's 502 while the registry is stopped, or a request for
    /// credentials, which piqueld has none of.
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
}

impl RegistryError {
    /// Whether the error is the registry's rather than one image's: it
    /// doesn't answer, refuses this client (401, 403), doesn't allow deletes
    /// (405), asks to slow down (429), or a proxy in front of it fails (502,
    /// 503, 504). Deleting any other image would fail the same way.
    #[must_use]
    pub fn is_registry_wide(&self) -> bool {
        match self {
            Self::Unreachable { .. } | Self::Unavailable { .. } => true,
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
}

impl BuildRegistry {
    /// Longest a connection to the registry may take before it counts as
    /// unreachable, as for the build machine.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Longest a whole request may take.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// A client of `repository`'s registry, which need not be reachable.
    ///
    /// # Errors
    /// Returns why the HTTP client could not be created.
    pub fn new(repository: BuildRepository) -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .timeout(Self::REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { repository, client })
    }

    /// Checks that the registry answers its API's base endpoint with
    /// success, within [`Self::CONNECT_TIMEOUT`]: image cleanup holds
    /// operations off while it waits.
    ///
    /// # Errors
    /// [`RegistryError::Unreachable`] when it doesn't answer, and
    /// [`RegistryError::Unavailable`] when it answers with an error.
    pub async fn check(&self) -> Result<(), RegistryError> {
        let response = self
            .client
            .get(format!("{}/v2/", self.base()))
            .timeout(Self::CONNECT_TIMEOUT)
            .send()
            .await
            .map_err(|source| self.unreachable(source))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(RegistryError::Unavailable {
            registry: self.repository.registry().to_owned(),
            status,
            message: RegistryAnswer::read(response).await.message(),
        })
    }

    /// Deletes `image` from the registry by digest. An image the registry
    /// says it doesn't have counts as deleted; any other 404, such as a
    /// proxy's, doesn't.
    ///
    /// # Errors
    /// Returns why the registry did not delete it.
    pub async fn delete(&self, image: &RepositoryDigest) -> Result<(), RegistryError> {
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
        if status.is_success() {
            return Ok(());
        }
        let answer = RegistryAnswer::read(response).await;
        if status == StatusCode::NOT_FOUND && answer.unknown() {
            return Ok(());
        }
        Err(RegistryError::Refused {
            image: image.clone(),
            status,
            message: answer.message(),
        })
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
    async fn read(response: reqwest::Response) -> Self {
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
