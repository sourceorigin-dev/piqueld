//! GitHub push webhooks. A delivery is authenticated by its HMAC-SHA256
//! signature, made with the application's secret, and then only hints that
//! the repository moved: sync lists the branches itself and never reads the
//! payload, so a delivery cannot choose what deploys.
use super::{ApplicationError, ApplicationService, Author};
use piqueld_core::ApplicationId;
use piqueld_core::sync::{WebhookSecret, WebhookView};

/// Why a delivery was refused.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    /// The signature is missing, malformed, or wrong, or the application has
    /// no secret or does not exist. These are indistinguishable to callers.
    #[error("the webhook signature does not verify")]
    Unauthorized,
    /// The secret could not be read.
    #[error(transparent)]
    Unavailable(#[from] ApplicationError),
}

/// What a verified delivery did.
#[derive(Debug, Eq, PartialEq)]
pub enum Delivery {
    /// A push: sync lists the repository's branches soon.
    Push,
    /// Any other event, such as GitHub's `ping`, which changes nothing.
    Ignored,
}

/// A delivery's `X-Hub-Signature-256` header, `sha256=<64 hex digits>`: the
/// HMAC-SHA256 of its body under the application's secret.
struct Signature([u8; 32]);

impl Signature {
    /// Parses the header value, or `None` when it is malformed.
    fn parse(header: &str) -> Option<Self> {
        let nibble = |digit: u8| {
            char::from(digit)
                .to_digit(16)
                .and_then(|value| u8::try_from(value).ok())
        };
        let (pairs, []) = header.strip_prefix("sha256=")?.as_bytes().as_chunks::<2>() else {
            return None;
        };
        let digest = pairs
            .iter()
            .map(|&[high, low]| Some(nibble(high)? << 4 | nibble(low)?))
            .collect::<Option<Vec<_>>>()?;
        Some(Self(digest.try_into().ok()?))
    }

    /// The signature of `body` under `secret`.
    #[expect(clippy::map_err_ignore, reason = "the error is the digest itself")]
    fn of(secret: &[u8], body: &[u8]) -> Result<Self, openssl::error::ErrorStack> {
        let key = openssl::pkey::PKey::hmac(secret)?;
        let digest = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key)?
            .sign_oneshot_to_vec(body)?;
        Ok(Self(
            digest
                .try_into()
                .map_err(|_| openssl::error::ErrorStack::get())?,
        ))
    }

    /// Whether this signs `body` under `secret`, compared in constant time.
    fn verifies(&self, secret: &[u8], body: &[u8]) -> bool {
        match Self::of(secret, body) {
            Ok(expected) => openssl::memcmp::eq(&expected.0, &self.0),
            Err(error) => {
                tracing::error!(?error, "webhook signature could not be computed");
                false
            }
        }
    }
}

/// The header form, `sha256=<hex>`, for tests sending deliveries.
#[cfg(test)]
impl std::fmt::Display for Signature {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sha256=")?;
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl ApplicationService {
    /// Verifies a GitHub delivery of `event` for `application`, signed by
    /// `signature`, and hints sync to list the repository for a push. The
    /// body is only authenticated, never interpreted.
    ///
    /// # Errors
    /// Returns `Unauthorized` unless the signature verifies with the
    /// application's secret, or `Unavailable` when it cannot be read.
    pub async fn receive_webhook(
        &self,
        application: &ApplicationId,
        signature: Option<&str>,
        event: Option<&str>,
        body: &[u8],
    ) -> Result<Delivery, WebhookError> {
        let signature = signature
            .and_then(Signature::parse)
            .ok_or(WebhookError::Unauthorized)?;
        let secret = self
            .store
            .webhook_secret(application)
            .await
            .map_err(ApplicationError::from)?
            .ok_or(WebhookError::Unauthorized)?;
        if !signature.verifies(&secret, body) {
            return Err(WebhookError::Unauthorized);
        }
        if event != Some("push") {
            return Ok(Delivery::Ignored);
        }
        self.hints.hint(application.clone());
        Ok(Delivery::Push)
    }

    /// Where GitHub sends `application`'s push webhooks, and when its secret
    /// was generated.
    ///
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn webhook(
        &self,
        application: &ApplicationId,
    ) -> Result<WebhookView, ApplicationError> {
        self.store.application(application).await?;
        Ok(WebhookView {
            url: self.webhook_url(application),
            secret_created_at_ms: self.store.webhook_secret_created(application).await?,
        })
    }

    /// Generates a new webhook secret for `application`, replacing the
    /// previous one, and returns it: it is never shown again.
    ///
    /// # Errors
    /// Returns refusal, absence, `Busy` during deletion, or storage errors.
    pub async fn generate_webhook_secret(
        &self,
        author: impl Into<Author<'_>>,
        application: &ApplicationId,
    ) -> Result<WebhookSecret, ApplicationError> {
        let (secret, created_at_ms) = self
            .store
            .generate_webhook_secret(author, application)
            .await?;
        Ok(WebhookSecret {
            url: self.webhook_url(application),
            secret: secret.to_string(),
            created_at_ms,
        })
    }

    /// Serves deliveries on `listener`, the webhook socket while ingress
    /// serves webhooks, until `cancellation`.
    pub(super) async fn serve_webhooks(
        &self,
        listener: Option<tokio::net::UnixListener>,
        cancellation: tokio_util::sync::CancellationToken,
    ) {
        let Some(listener) = listener else {
            return;
        };
        let router = crate::api::http::webhook_router(self.clone());
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(cancellation.cancelled_owned())
            .await
        {
            tracing::error!(?error, "webhook listener failed");
        }
    }

    /// The payload URL of `application`'s webhooks, when the daemon serves
    /// them on a public hostname.
    fn webhook_url(&self, application: &ApplicationId) -> Option<String> {
        let hostname = self.webhook_hostname.as_ref()?;
        Some(format!("https://{hostname}{WEBHOOK_PATH}{application}"))
    }
}

/// Path prefix of webhook deliveries, followed by the application ID.
pub const WEBHOOK_PATH: &str = "/hooks/github/";
/// Largest delivery accepted, in bytes. GitHub's push payloads are far
/// smaller unless a push carries thousands of commits.
pub const WEBHOOK_BODY_LIMIT: usize = 1024 * 1024;

#[cfg(test)]
mod tests;
