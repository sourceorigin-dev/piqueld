//! The managed gateway, its routes, DNS records and certificates.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::names::string_enum;

/// Managed gateway health, per listener, and route diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct IngressStatus {
    /// Effective read-only daemon setting.
    pub enabled: bool,
    /// Whether the gateway has accepted its desired configuration (or is
    /// stopped), and in tunnel mode whether the tunnel is connected. Public
    /// routes depend only on this.
    pub healthy: bool,
    /// Whether the gateway accepted its desired configuration (or is
    /// stopped), whatever one application's network does. Unlike `healthy`,
    /// another application's failure does not clear it.
    #[serde(default)]
    pub gateway: bool,
    /// Safe diagnostic, with detailed causes in daemon logs.
    pub message: String,
    /// How public routes reach the gateway.
    #[serde(default)]
    pub public: PublicIngressStatus,
    /// The private listener and the apps tailnet node carrying its traffic.
    #[serde(default)]
    pub private: PrivateIngressStatus,
    /// Deployed routes and their latest independent HTTPS probes, limited to
    /// applications the caller can read.
    pub routes: Vec<RouteStatus>,
}

/// How public routes reach the gateway: `[ingress.tunnel]` selects the mode.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PublicIngressStatus {
    /// The gateway publishes ports 80 and 443 on the host.
    #[default]
    Direct,
    /// `cloudflared` connects out to Cloudflare, which terminates TLS and
    /// forwards to the gateway. No inbound port is open.
    Tunnel {
        /// Tunnel ID from its credentials file.
        id: String,
        /// `cloudflared` holds a connection to Cloudflare's edge, from its
        /// healthcheck of `/ready`.
        connected: bool,
        /// Safe diagnostic, with detailed causes in daemon logs.
        message: String,
    },
}

/// The gateway's private listener, reached through the apps tailnet node.
/// A broken node degrades only private routes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct PrivateIngressStatus {
    /// Effective read-only daemon setting, `[ingress.private] enabled`.
    pub enabled: bool,
    /// The node is logged in and the gateway serves the private listener.
    pub healthy: bool,
    /// Tailscale backend state of the node, such as `Running` or `NeedsLogin`.
    pub state: String,
    /// The node's fully qualified `MagicDNS` name.
    pub dns_name: Option<String>,
    /// The node's tailnet addresses, which private hostnames must resolve to.
    pub addresses: Vec<String>,
    /// Safe diagnostic: what to do while the node needs login (its login URL
    /// is only logged), or the failed step, with detailed causes in daemon
    /// logs.
    pub message: String,
}

/// The records a route's hostname needs. piqueld creates them where its DNS
/// provider manages records (see [`DnsRecordState`]); otherwise operators
/// copy them to their DNS provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DnsRecords {
    /// Public routes: A/AAAA records to this server's public addresses.
    ServerAddresses {
        /// `[ingress] public_addresses`; empty when not configured.
        #[serde(default)]
        addresses: Vec<String>,
    },
    /// Public routes in tunnel mode: a proxied CNAME record to the tunnel.
    TunnelCname {
        /// `<tunnel-id>.cfargotunnel.com`.
        target: String,
    },
    /// Private routes: the apps tailnet node's addresses, empty until it has
    /// joined the tailnet.
    TailnetAddresses {
        /// IPv4 and IPv6 tailnet addresses.
        addresses: Vec<String>,
    },
}

/// `A/AAAA -> 192.0.2.1`, the tunnel's CNAME, or the tailnet addresses.
impl std::fmt::Display for DnsRecords {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerAddresses { addresses } if addresses.is_empty() => {
                formatter.write_str("A/AAAA -> this server's public addresses")
            }
            Self::TunnelCname { target } => write!(formatter, "CNAME (proxied) -> {target}"),
            Self::TailnetAddresses { addresses } if addresses.is_empty() => {
                formatter.write_str("A/AAAA -> the apps node's tailnet addresses, once it joins")
            }
            Self::ServerAddresses { addresses } | Self::TailnetAddresses { addresses } => {
                write!(formatter, "A/AAAA -> {}", addresses.join(", "))
            }
        }
    }
}

string_enum! {
    #[serde]
    /// Who keeps a route's DNS records current.
    #[derive(Default)]
    pub enum DnsRecordState {
        /// The operator: no provider with `manage_records` hosts the hostname's
        /// zone, or a direct public route has no `[ingress] public_addresses`.
        #[default]
        Manual => "manual",
        /// piqueld created the records and keeps them current.
        Managed => "managed",
        /// piqueld will manage the records but has not written them yet: the
        /// gateway has not applied the route, the apps node has no addresses
        /// yet, or the provider failed and the change is retried.
        Pending => "pending",
        /// A record piqueld does not own already exists at the hostname. piqueld
        /// never overwrites it; remove it to let piqueld manage the hostname.
        DnsConflict => "dns_conflict",
    }
}

/// Login and certificate state of the daemon's own tailnet node.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TailnetStatus {
    /// Effective read-only daemon setting.
    pub enabled: bool,
    /// The node is logged in and serves an unexpired certificate.
    pub healthy: bool,
    /// Tailscale backend state, such as `Running` or `NeedsLogin`.
    pub state: String,
    /// The node's fully qualified `MagicDNS` name.
    pub dns_name: Option<String>,
    /// Expiry of the served HTTPS certificate.
    pub certificate_expires_at_ms: Option<i64>,
    /// Whether `auth.public_url` is the node's HTTPS origin.
    pub public_url_matches: bool,
    /// Safe diagnostic, with detailed causes in daemon logs.
    pub message: String,
}

/// DNS providers from daemon TOML and the DNS-01 certificates piqueld issues
/// through them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DnsStatus {
    /// Configured providers, in configuration order.
    pub providers: Vec<DnsProviderStatus>,
    /// Certificates that routes need, or that are kept until they expire.
    pub certificates: Vec<CertificateStatus>,
}

/// Zone discovery state of one configured DNS provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DnsProviderStatus {
    /// Provider kind, such as `cloudflare` or `ovh`.
    pub kind: String,
    /// piqueld manages routes' DNS records in this provider's zones.
    #[serde(default)]
    pub manage_records: bool,
    /// Zones discovered through the provider's API, including conflicting ones.
    pub zones: Vec<String>,
    /// The latest discovery succeeded and no zone is claimed by another provider.
    pub healthy: bool,
    /// Safe diagnostic: the discovery error or the conflicting zones.
    pub message: String,
}

/// One certificate issued through DNS-01, which covers a single name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CertificateStatus {
    /// Covered name: a wildcard such as `*.example.com`, or an exact hostname.
    pub name: String,
    /// Route hostnames served with it; empty once no route needs it.
    pub hostnames: Vec<String>,
    /// Expiry of the stored certificate, absent until one is issued.
    pub expires_at_ms: Option<i64>,
    /// Latest issuance or renewal failure, cleared by the next success.
    pub error: Option<String>,
}

/// HTTPS readiness is separate from application rollout success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RouteStatus {
    /// Owning environment identity.
    pub environment_id: String,
    /// The route's name, when it has one.
    #[serde(default)]
    pub name: Option<crate::RouteName>,
    /// Exact public DNS hostname.
    pub hostname: String,
    /// Effective visibility, which selects the listener serving the route.
    pub visibility: crate::manifest::Visibility,
    /// The records the hostname needs.
    pub dns: DnsRecords,
    /// Whether piqueld manages those records.
    #[serde(default)]
    pub dns_state: DnsRecordState,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: crate::manifest::RouteTarget,
    /// disabled, pending, ready, or failed.
    pub state: String,
    /// Diagnostic explaining DNS, TLS, or gateway readiness.
    pub message: String,
}
