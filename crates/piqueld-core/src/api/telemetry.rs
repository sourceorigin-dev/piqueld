//! Export of the daemon's telemetry, and environments receiving it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{EnvironmentId, EnvironmentName};

/// Export of the daemon's traces, metrics and logs over OTLP, and of its
/// containers' logs and resource usage.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TelemetryStatus {
    /// Collectors receiving telemetry: the `[telemetry]` endpoint, if set,
    /// then the receiving service of each granted environment the caller
    /// can read.
    pub destinations: Vec<TelemetryDestinationStatus>,
    /// Whether the logs and resource usage of the containers piqueld runs are
    /// exported along with the daemon's own telemetry.
    #[serde(default)]
    pub containers: bool,
}

impl TelemetryStatus {
    /// What is exported, as status output describes it.
    #[must_use]
    pub const fn exported(&self) -> &'static str {
        if self.containers {
            "daemon traces, metrics and logs; container logs and resource usage"
        } else {
            "daemon traces, metrics and logs"
        }
    }
}

/// One collector receiving the daemon's telemetry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TelemetryDestinationStatus {
    /// OTLP/gRPC endpoint: `https://`, `http://` or `unix://`.
    pub endpoint: String,
    /// The file request headers were read from, such as
    /// `from /run/credentials/piqueld.service/otlp-headers`; never their
    /// values. Absent without headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<String>,
    /// The environment whose receiving service this is; absent for the
    /// configured endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<TelemetryReceiver>,
    /// When the collector last accepted an export, in Unix milliseconds;
    /// absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_ms: Option<i64>,
    /// Records dropped since the daemon started, because the queue was full
    /// or the collector failed or rejected them.
    pub dropped: u64,
    /// Whether the collector accepted the last export.
    #[serde(default)]
    pub connected: bool,
    /// Whether exports have failed for at least a minute without a success.
    pub failing: bool,
}

impl TelemetryDestinationStatus {
    /// How export is going, in a few words: failing for a minute,
    /// connected, reconnecting after a success, or not connected yet.
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match (self.failing, self.connected, self.last_success_ms) {
            (true, _, _) => "failing",
            (false, true, _) => "connected",
            (false, false, Some(_)) => "reconnecting",
            (false, false, None) => "not connected yet",
        }
    }
}

/// The environment a telemetry destination is the receiving service of.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TelemetryReceiver {
    /// Stable environment ID.
    pub environment_id: EnvironmentId,
    /// Its application's current name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<crate::ApplicationName>,
    /// Its current name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentName>,
}

impl TelemetryReceiver {
    /// Environment `environment_id`, before its names are looked up.
    #[must_use]
    pub const fn new(environment_id: EnvironmentId) -> Self {
        Self {
            environment_id,
            application: None,
            environment: None,
        }
    }
}

/// Grants or revokes an environment's telemetry.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentTelemetryRequest {
    /// Whether its receiving service gets every application's telemetry.
    pub granted: bool,
}

/// Whether an environment receives the daemon's telemetry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TelemetryReport {
    /// Whether the environment is granted the daemon's telemetry.
    pub granted: bool,
    /// The deployed service receiving it, if the current runtime target has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<crate::ServiceName>,
    /// The daemon's export to that service, while it is granted and deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<TelemetryDestinationStatus>,
}
