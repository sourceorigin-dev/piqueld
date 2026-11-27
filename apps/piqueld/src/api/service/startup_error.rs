//! Why the application service could not be built or started.

use crate::{auth::AuthError, ingress::IngressError};
use piqueld_config::InvalidOrigin;
use piqueld_docker::{DockerError, RegisterError};
use piqueld_store::StoreError;
use thiserror::Error;

/// Why the application service could not be built or started.
#[derive(Debug, Error)]
pub enum StartupError {
    /// The control-plane state could not be opened.
    #[error("failed to open control-plane state")]
    OpenStore(#[source] StoreError),
    /// The control-plane state failed.
    #[error("{0}")]
    Store(#[from] StoreError),
    /// Authentication could not be initialized.
    #[error("{0}")]
    Auth(#[from] AuthError),
    /// `auth.public_url` is not a valid origin.
    #[error("{0}")]
    PublicUrl(#[from] InvalidOrigin),
    /// Ingress could not be configured.
    #[error("{0}")]
    Ingress(#[from] IngressError),
    /// The webhook socket could not be bound.
    #[error("failed to bind the webhook socket")]
    BindWebhooks(#[source] IngressError),
    /// The Git checkouts and snapshots directories could not be prepared.
    #[error("failed to prepare the Git checkouts and snapshots directories")]
    PrepareSources(#[source] std::io::Error),
    /// The daemon's start could not be recorded.
    #[error("failed to record the daemon start")]
    RecordStart(#[source] StoreError),
    /// The Docker Engine client could not be configured.
    #[error("failed to configure the Docker Engine client")]
    ConfigureDocker(#[source] DockerError),
    /// The build machine could not be registered with Buildx.
    #[error("failed to register the build machine with Docker Buildx")]
    RegisterBuilder(#[source] RegisterError),
    /// The notification webhook client could not be built.
    #[error("initialize the notification webhook client")]
    NotificationClient(#[source] reqwest::Error),
}
