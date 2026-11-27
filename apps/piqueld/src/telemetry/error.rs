//! Why telemetry export could not be prepared.

use piqueld_config::HeadersError;
use thiserror::Error;

/// Why telemetry export could not be prepared.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// The endpoint is not a valid URI.
    #[error("invalid telemetry endpoint")]
    Endpoint(#[source] tonic::transport::Error),
    /// The TLS configuration for an `https` endpoint was rejected.
    #[error("invalid telemetry TLS configuration")]
    Tls(#[source] tonic::transport::Error),
    /// The headers file does not parse.
    #[error(transparent)]
    Headers(#[from] HeadersError),
}
