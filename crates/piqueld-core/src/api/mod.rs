//! Shared requests and responses for the versioned HTTP API.
//!
//! The submodules group them by what they describe; everything is re-exported
//! here, so callers name them `api::<Type>`.

mod applications;
mod builds;
mod deployments;
mod environments;
mod error_code;
mod ingress;
mod logs;
mod previews;
mod secrets;
mod system;
mod telemetry;
#[cfg(test)]
mod tests;

pub use applications::*;
pub use builds::*;
pub use deployments::*;
pub use environments::*;
pub use error_code::*;
pub use ingress::*;
pub use logs::*;
pub use previews::*;
pub use secrets::*;
pub use system::*;
pub use telemetry::*;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use crate::urls::{RouteUrl, UrlCondition, UrlState};

/// Page size of a paginated listing that requests none, except the
/// application list, which defaults to [`MAX_PAGE_SIZE`].
pub const DEFAULT_PAGE_SIZE: u16 = 50;
/// Largest page size any paginated listing accepts.
pub const MAX_PAGE_SIZE: u16 = 100;
/// Largest secret value, in bytes (500 KiB); values must not be empty.
pub const MAX_SECRET_VALUE_BYTES: usize = 500 * 1024;
/// Largest API request body, in bytes (2 MiB). Manifests are the largest
/// payload, so this is also the manifest size limit: the CLI checks uploads
/// against it and the daemon applies it to manifests read from repositories.
pub const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Successful API response envelope.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Envelope<T> {
    /// Response payload.
    pub data: T,
}

/// Cursor-paginated API response.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Page<T> {
    /// Items in this page.
    pub items: Vec<T>,
    /// Cursor for the next page, when more items are available.
    pub next_cursor: Option<String>,
}

/// Structured error returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable error code; `ApiErrorCode` lists them. A
    /// client should expect codes it doesn't know yet.
    pub code: String,
    /// Safe human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    /// Server-generated request identifier.
    #[serde(default)]
    #[schema(required = true)]
    pub request_id: String,
}
