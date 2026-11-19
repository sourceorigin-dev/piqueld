//! Control-plane status, readiness and installation checks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{DnsStatus, IngressStatus, PreviewUsage, TailnetStatus, TelemetryStatus};

/// Current control-plane status.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SystemStatus {
    /// Machine-readable service status.
    pub status: String,
    /// Version of the exposed API.
    pub api_version: String,
    /// Version of the running daemon binary.
    pub daemon_version: String,
    /// Control-plane instance identifier.
    pub instance_id: String,
    /// Dedicated tailnet node serving the website over HTTPS.
    #[serde(default)]
    pub tailscale: TailnetStatus,
    /// DNS providers and the certificates issued through them with DNS-01.
    #[serde(default)]
    pub dns: DnsStatus,
    /// Previews against the `[previews]` limits; absent when they could not
    /// be counted.
    #[serde(default)]
    pub previews: Option<PreviewUsage>,
    /// The images this installation built, and their cleanup.
    #[serde(default)]
    pub images: ImageStatus,
    /// How large a local snapshot may be; absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotLimits>,
    /// Export of the daemon's own telemetry over OTLP.
    #[serde(default)]
    pub telemetry: TelemetryStatus,
    /// The build machine images build on, when the host has one and the
    /// caller has `system:read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<BuilderStatus>,
}

/// A build machine: where it is, and whether its `BuildKit` daemon answered.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuilderStatus {
    /// Its `BuildKit` daemon, as `tcp://host:port`.
    pub endpoint: String,
    /// The image repository it pushes to, and the host pulls from.
    pub repository: String,
    /// The latest check of its `BuildKit` daemon, before a build or on
    /// request; absent until the first.
    pub last_check: Option<BuilderCheck>,
}

/// One check of a build machine's `BuildKit` daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuilderCheck {
    /// Whether it accepted a connection.
    pub reachable: bool,
    /// When it was checked, in Unix milliseconds.
    pub checked_at_ms: i64,
    /// Why connecting failed, such as a refused connection or a timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// How large a snapshot `piquelctl preview deploy --local` uploads may be,
/// so it can refuse one before writing it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SnapshotLimits {
    /// The largest bundle an upload may carry: `[snapshots] max_upload_bytes`.
    pub max_upload_bytes: u64,
    /// The most bytes its files may take, each file and directory charged
    /// [`Self::file_cost`]: `[checkouts] max_bytes`.
    pub max_tree_bytes: u64,
    /// The most files and directories it may have.
    pub max_files: u64,
}

impl SnapshotLimits {
    /// Filesystem block size files are charged in.
    const BLOCK: u64 = 4096;
    /// Each file's share of Git's index.
    const METADATA: u64 = 256;

    /// The disk a checked-out file of `size` bytes, or a directory of
    /// none, is charged: whole blocks, at least one, and its share of the
    /// index.
    ///
    /// ```
    /// use piqueld_core::api::SnapshotLimits;
    /// assert_eq!(SnapshotLimits::file_cost(1), 4096 + 256);
    /// assert_eq!(SnapshotLimits::file_cost(4097), 8192 + 256);
    /// ```
    #[must_use]
    pub const fn file_cost(size: u64) -> u64 {
        let blocks = if size == 0 {
            1
        } else {
            size.div_ceil(Self::BLOCK)
        };
        blocks
            .saturating_mul(Self::BLOCK)
            .saturating_add(Self::METADATA)
    }
}

/// The images this installation built, as of the last cleanup.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ImageStatus {
    /// Built images still present after the last cleanup.
    pub images: u32,
    /// Total size of the images cleanup removed since the daemon started.
    /// Layers they shared with kept images stay, so less space may be freed.
    pub reclaimed_bytes: u64,
    /// When cleanup last ran, in Unix milliseconds; absent before it first
    /// ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned_at_ms: Option<i64>,
    /// The images this host's builds pushed to the build registry,
    /// as of the last cleanup; absent without a build machine, and before
    /// cleanup first ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryImageStatus>,
}

/// The images this host's builds pushed to the build registry,
/// which cleanup deletes once no deployment it keeps there uses them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RegistryImageStatus {
    /// The repository they are in, as `build.repository` names it.
    pub repository: String,
    /// Pushed images still in the registry as of the last cleanup that
    /// listed it; absent before one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<u32>,
    /// Images cleanup deleted from the registry since the daemon started.
    /// The registry's garbage collection frees their layers.
    pub deleted_images: u32,
    /// Why the last cleanup didn't delete every image no deployment keeps:
    /// the registry didn't answer, or refused a deletion. Absent when it
    /// deleted them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Effective host settings loaded by the daemon; no mutation endpoint exists.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct HostConfiguration {
    /// Settings grouped by server, Docker, reconciliation and retention.
    pub groups: BTreeMap<String, BTreeMap<String, String>>,
}

/// One diagnostic dependency probe, independent of application health.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DependencyStatus {
    /// The dependency is usable.
    Ready,
    /// The probe failed with a safe explanation.
    Failed {
        /// Explanation when the probe fails.
        message: String,
    },
}

impl DependencyStatus {
    /// Creates a dependency verdict with a safe failure explanation.
    #[must_use]
    pub fn new(ready: bool, failure: &str) -> Self {
        if ready {
            Self::Ready
        } else {
            Self::Failed {
                message: failure.to_owned(),
            }
        }
    }
}

/// Deployment prerequisites. Configuration APIs remain available when false.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ReadinessStatus {
    /// All deployment dependencies are ready.
    pub ready: bool,
    /// `SQLite` can execute a read query.
    pub database: DependencyStatus,
    /// Docker answers its ping endpoint.
    pub docker: DependencyStatus,
    /// Docker is a compatible single-node Swarm manager.
    pub swarm: DependencyStatus,
    /// Managed ingress is independent of core deployment dependencies.
    #[serde(default)]
    pub ingress: IngressStatus,
}

/// Installation health from `GET /api/v1/system/checks`, rendered by
/// `piquelctl doctor` and the dashboard. Each check runs against the live
/// installation with a bounded timeout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SystemChecks {
    /// One verdict per check, in a stable order.
    pub checks: Vec<SystemCheck>,
}

impl SystemChecks {
    /// Whether any check failed, which makes `piquelctl doctor` exit nonzero.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
    }
}

/// One check's verdict, what was found, and what to do about it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SystemCheck {
    /// Which check this is.
    pub id: SystemCheckId,
    /// The verdict.
    pub status: CheckStatus,
    /// What was found, without secrets.
    pub message: String,
    /// What to do next; absent when nothing is needed.
    pub next_step: Option<String>,
}

impl SystemCheck {
    /// A verdict needing no action.
    #[must_use]
    pub fn new(id: SystemCheckId, status: CheckStatus, message: impl Into<String>) -> Self {
        Self {
            id,
            status,
            message: message.into(),
            next_step: None,
        }
    }

    /// Adds what the operator should do next.
    #[must_use]
    pub fn next_step(mut self, step: impl Into<String>) -> Self {
        self.next_step = Some(step.into());
        self
    }
}

/// Identifies a check. New features add their own variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemCheckId {
    /// `auth.public_url` is HTTPS and reachable from the daemon.
    PublicUrl,
    /// Docker Engine and API versions meet ingress requirements.
    Docker,
    /// Ports 80 and 443 are free, or held by the managed gateway.
    IngressPorts,
    /// `secrets.key` is private and matches the stored verifier.
    SecretKey,
    /// The runtime directory and API socket have the expected permissions.
    RuntimeDirectory,
    /// The daemon's tailnet node is logged in with a valid certificate.
    Tailnet,
    /// Backups are recent.
    Backups,
}

impl SystemCheckId {
    /// Display name, e.g. `Public URL`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PublicUrl => "Public URL",
            Self::Docker => "Docker Engine",
            Self::IngressPorts => "Ports 80/443",
            Self::SecretKey => "Secret key",
            Self::RuntimeDirectory => "API socket",
            Self::Tailnet => "Tailnet node",
            Self::Backups => "Backups",
        }
    }
}

/// A check's verdict. Only `fail` makes `piquelctl doctor` exit nonzero.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// As expected.
    Pass,
    /// Works, but likely not as intended.
    Warn,
    /// Broken; follow the next step.
    Fail,
    /// Not applicable to this configuration, or not available to check.
    Skip,
}

impl CheckStatus {
    /// Lowercase name, as serialized.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}
