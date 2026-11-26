//! Why the gateway, its certificates, its DNS records and its route probes fail.

use super::{gateway::UnmetRequirement, node::TailnetOverlap};
use crate::certificate::LeafError;
use piqueld_config::{
    directories::PrivateFileError,
    dns::{PropagationError, ZoneError, provider::DnsError},
    tailscale::LocalApiError,
    unix_http::UnixHttpError,
};
use piqueld_core::ErrorChain as _;
use piqueld_docker::DockerError;
use piqueld_store::StoreError;
use std::{io, net::AddrParseError, time::SystemTimeError};
use thiserror::Error;
use tokio::time::error::Elapsed;

/// Why a Unix control socket of the gateway could not be bound.
#[derive(Debug, Error)]
pub enum ControlError {
    /// The data or control directory could not be prepared.
    #[error("{0}")]
    Prepare(#[source] io::Error),
    /// The socket an earlier run left could not be removed.
    #[error("remove the previous {name}")]
    RemovePrevious {
        /// The socket's file name.
        name: String,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// The socket could not be bound.
    #[error("bind {name}")]
    Bind {
        /// The socket's file name.
        name: String,
        /// The failure.
        #[source]
        source: io::Error,
    },
}

/// Why ingress could not reconcile, certify, probe or stop its gateway.
#[derive(Debug, Error)]
pub enum IngressError {
    /// The store failed.
    #[error("{0}")]
    Store(#[from] StoreError),
    /// Docker failed.
    #[error("{0}")]
    Docker(#[from] DockerError),
    /// Caddy's admin API failed.
    #[error("{0}")]
    Caddy(#[from] UnixHttpError),
    /// A file system operation failed.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// A JSON document could not be encoded or decoded.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// A private file could not be written or removed.
    #[error("{0}")]
    PrivateFile(#[from] PrivateFileError),
    /// The clock is before the Unix epoch.
    #[error("{0}")]
    Clock(#[from] SystemTimeError),
    /// The ACME client could not be built or loaded.
    #[error("{0}")]
    AcmeClient(#[from] instant_acme::Error),
    /// A DNS provider failed.
    #[error("{0}")]
    Dns(#[from] DnsError),
    /// No zone serves a hostname.
    #[error("{0}")]
    Zone(#[from] ZoneError),
    /// A record's propagation could not be checked.
    #[error("{0}")]
    Propagation(#[from] PropagationError),
    /// A control socket could not be bound.
    #[error("{0}")]
    Control(#[from] ControlError),
    /// A requirement of the gateway's host is not met.
    #[error("{0}")]
    Unmet(#[from] UnmetRequirement),
    /// An ingress network's subnet overlaps the tailnet.
    #[error("{0}")]
    TailnetOverlap(#[from] TailnetOverlap),
    /// A route probe's HTTPS client failed.
    #[error("{0}")]
    Probe(#[from] reqwest::Error),
    /// The Docker socket path is not UTF-8.
    #[error("Docker socket is not UTF-8")]
    Socket(#[source] DockerError),
    /// The response from Caddy is not JSON.
    #[error("decode ingress HTTP response")]
    DecodeResponse(#[source] serde_json::Error),
    /// The ingress containers could not be listed.
    #[error("list the running ingress containers")]
    ListContainers(#[source] DockerError),
    /// An ingress container could not be stopped.
    #[error("stop the ingress container {id}")]
    StopContainer {
        /// The container.
        id: String,
        /// The failure.
        #[source]
        source: DockerError,
    },
    /// The traffic log's socket could not be bound.
    #[error("failed to bind the gateway's traffic socket")]
    BindTraffic(#[source] ControlError),
    /// A gateway pass failed at `stage`.
    #[error("{stage}")]
    Stage {
        /// What the pass was doing.
        stage: &'static str,
        /// The failure.
        #[source]
        source: Box<IngressError>,
    },
    /// An application's ingress network is unavailable.
    #[error("application {id} ingress network: {message}")]
    ApplicationNetwork {
        /// The application's environment.
        id: piqueld_core::EnvironmentId,
        /// Why, as the failure's text.
        message: String,
    },
    /// The apps node's status could not be read.
    #[error("read the apps node status")]
    NodeStatus(#[source] LocalApiError),
    /// The unconfigured auth key could not be removed.
    #[error("remove the unconfigured auth key")]
    RemoveAuthKey(#[source] PrivateFileError),
    /// The tunnel's credentials could not be removed.
    #[error("remove the tunnel credentials written for cloudflared")]
    RemoveTunnelCredentials(#[source] PrivateFileError),
    /// There is no `cloudflared` container.
    #[error("cloudflared does not exist")]
    CloudflaredMissing,
    /// `cloudflared` is not running.
    #[error("cloudflared is not running")]
    CloudflaredStopped,
    /// `cloudflared` reports no health.
    #[error("cloudflared reports no health status")]
    CloudflaredNoHealth,
    /// The DNS change could not be journaled.
    #[error("record the DNS change in the journal")]
    JournalDns(#[source] Box<IngressError>),
    /// A resource under a managed name belongs to another installation.
    #[error("gateway resource is not owned by this installation")]
    NotOwned,
    /// Docker reported a network without an ID.
    #[error("network ID missing")]
    NetworkIdMissing,
    /// Docker reported a container without an ID.
    #[error("container ID missing")]
    ContainerIdMissing,
    /// The edge network does not exist.
    #[error("gateway edge network is not ready")]
    EdgeNotReady,
    /// The edge network has no subnet.
    #[error("gateway edge network has no subnet")]
    EdgeNoSubnet,
    /// The edge network conflicts with the gateway's.
    #[error("gateway egress network configuration conflicts")]
    EdgeConflict,
    /// The gateway container does not run.
    #[error("gateway container is not running")]
    GatewayStopped,
    /// The gateway container vanished.
    #[error("gateway container disappeared")]
    GatewayDisappeared,
    /// The gateway has no address on the edge network.
    #[error("gateway has no edge network address")]
    NoEdgeAddress,
    /// The gateway's edge address is not an IP address.
    #[error("decode the gateway's edge network address")]
    DecodeEdgeAddress(#[source] AddrParseError),
    /// An application's ingress network does not exist.
    #[error("application ingress network is not ready")]
    IngressNetworkNotReady,
    /// An application's ingress network belongs to someone else.
    #[error("application ingress network has conflicting ownership")]
    IngressNetworkOwnership,
    /// An application's ingress network is not an attachable overlay.
    #[error("application ingress network must be an attachable overlay")]
    IngressNetworkKind,
    /// An application's ingress network has no subnet.
    #[error("ingress network has no subnet to check against the tailnet ranges")]
    IngressNetworkNoSubnet,
    /// A subnet Docker reported is not valid.
    #[error("decode ingress network subnet {subnet}")]
    DecodeSubnet {
        /// The subnet as Docker reported it.
        subnet: String,
    },
    /// Pulling an image outlived its timeout.
    #[error("pull {image} timed out")]
    PullTimedOut {
        /// The image.
        image: String,
        /// The elapsed timeout.
        #[source]
        source: Elapsed,
    },
    /// The gateway cannot be replaced while its retained networks are unverified.
    #[error(
        "cannot replace gateway: retained routes for application {id} lack a verified network and the old gateway is not running"
    )]
    ReplaceNotRunning {
        /// The application.
        id: piqueld_core::EnvironmentId,
    },
    /// The gateway cannot switch modes while its retained networks are unverified.
    #[error(
        "cannot switch between direct and tunnel ingress: retained routes for application {id} lack a verified network; repair it or withdraw its routes"
    )]
    SwitchBlocked {
        /// The application.
        id: piqueld_core::EnvironmentId,
    },
    /// A replacement failed and the previous gateway was restored.
    #[error("gateway replacement failed; previous gateway restored")]
    ReplacementFailed(#[source] Box<IngressError>),
    /// A replacement failed and the previous gateway could not be restored.
    #[error("gateway replacement failed ({}); rollback also failed", replacement.chain())]
    RollbackFailed {
        /// Why the replacement failed.
        replacement: Box<IngressError>,
        /// Why the rollback failed.
        #[source]
        source: Box<IngressError>,
    },
    /// The validation container's diagnostics could not be read.
    #[error("replacement Caddy configuration validation failed; could not read diagnostics")]
    ValidationDiagnostics(#[source] DockerError),
    /// The candidate Caddy configuration is invalid.
    #[error("replacement Caddy configuration validation failed: {diagnostics}")]
    ValidationFailed {
        /// The container's last log lines.
        diagnostics: String,
    },
    /// Caddy's admin API never answered after the gateway started.
    #[error("Caddy administration did not become ready after startup; inspect gateway logs")]
    CaddyNotReady,
    /// A managed container's setting differs from the spec.
    #[error("managed container {field} conflicts with managed configuration")]
    ContainerField {
        /// The setting.
        field: &'static str,
    },
    /// A managed container's restart policy differs.
    #[error("managed container restart policy conflicts")]
    RestartPolicy,
    /// A managed container's host configuration differs.
    #[error("managed container host configuration {field} conflicts")]
    HostConfiguration {
        /// The setting.
        field: &'static str,
    },
    /// A managed container's port bindings differ.
    #[error("managed container port bindings conflict")]
    PortBindings,
    /// A managed container reports no environment.
    #[error("managed container environment is absent")]
    EnvironmentAbsent,
    /// The managed spec has no environment.
    #[error("managed environment is absent")]
    SpecEnvironmentAbsent,
    /// A managed container's environment differs.
    #[error("managed container environment conflicts")]
    EnvironmentConflict,
    /// A managed container is privileged.
    #[error("managed containers must not be privileged")]
    Privileged,
    /// A stored certificate has no private key.
    #[error("no PKCS#8 private key")]
    NoPrivateKey,
    /// A certificate's leaf could not be read; its text names the part.
    #[error("decode {0}")]
    DecodeCertificate(LeafError),
    /// An ACME call was cancelled by the daemon's shutdown.
    #[error("{step}: cancelled by daemon shutdown")]
    AcmeShutdown {
        /// The ACME step.
        step: &'static str,
    },
    /// An ACME call did not answer in time.
    #[error("{step}: no answer within {seconds}s")]
    AcmeTimedOut {
        /// The ACME step.
        step: &'static str,
        /// The limit, in seconds.
        seconds: u64,
        /// The elapsed timeout.
        #[source]
        source: Elapsed,
    },
    /// An ACME call failed.
    #[error("{step}")]
    Acme {
        /// The ACME step.
        step: &'static str,
        /// The failure.
        #[source]
        source: instant_acme::Error,
    },
    /// An expired certificate could not be deleted.
    #[error("delete expired certificate")]
    DeleteCertificate(#[source] PrivateFileError),
    /// The challenge record an earlier attempt left could not be deleted.
    #[error("delete the challenge record left by an earlier attempt")]
    DeleteLeftoverChallenge(#[source] Box<IngressError>),
    /// The ACME order has no authorization.
    #[error("ACME order has no authorization")]
    NoAuthorization,
    /// The CA offered no DNS-01 challenge.
    #[error("the CA offered no DNS-01 challenge")]
    NoDns01Challenge,
    /// The shutdown cancelled waiting for a DNS record to propagate.
    #[error("cancelled by daemon shutdown")]
    Shutdown,
    /// The ACME authorization is neither valid nor pending.
    #[error("ACME authorization is {status}")]
    AuthorizationStatus {
        /// The status, as `Debug` prints it.
        status: String,
    },
    /// The ACME order is not ready.
    #[error("ACME order is {status}{problem}")]
    OrderNotReady {
        /// The status, as `Debug` prints it.
        status: String,
        /// `: <problem>` when the CA gave one, else empty.
        problem: String,
    },
    /// The stored ACME account does not decode.
    #[error("decode stored ACME account")]
    DecodeAccount(#[source] serde_json::Error),
    /// A route has no served hostname.
    #[error("every one-character name below the wildcard has its own exact route")]
    NoServedHostname,
    /// A route check outlived its timeout.
    #[error("the route check took over 10s")]
    RouteCheckTimedOut(#[source] Elapsed),
    /// A route's hostname does not resolve.
    #[error("resolve {hostname}")]
    Resolve {
        /// The hostname.
        hostname: String,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// A private route's DNS does not name the apps node.
    #[error("DNS answers {resolved:?} instead of the apps node's addresses {expected:?}")]
    DnsMismatch {
        /// What the hostname resolved to.
        resolved: std::collections::BTreeSet<std::net::IpAddr>,
        /// The apps node's addresses.
        expected: std::collections::BTreeSet<std::net::IpAddr>,
    },
    /// The private probe's PROXY relay could not start.
    #[error("start the private probe's PROXY relay")]
    ProxyRelay(#[source] io::Error),
    /// The probe's HTTPS request got a failing status.
    #[error("HTTPS probe returned {0}")]
    ProbeStatus(reqwest::StatusCode),
    /// The probe's body is longer than the instance identity.
    #[error("HTTPS probe body exceeds its limit")]
    ProbeBody,
    /// The probed address serves another gateway.
    #[error("DNS points at another server or gateway")]
    OtherGateway,
}

impl IngressError {
    /// The unmet host requirement behind this failure, if one is.
    #[must_use]
    pub fn unmet(&self) -> Option<&UnmetRequirement> {
        match self {
            Self::Unmet(unmet) => Some(unmet),
            Self::Stage { source, .. } | Self::ReplacementFailed(source) => source.unmet(),
            _ => None,
        }
    }

    /// Whether an ingress network overlaps the tailnet.
    #[must_use]
    pub const fn overlaps_tailnet(&self) -> bool {
        matches!(self, Self::TailnetOverlap(_))
    }
}
