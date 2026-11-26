//! Read-only host configuration for the piqueld daemon: its configuration
//! file, the DNS provider accounts it names, the private state and runtime
//! directories it prepares, and the clients of the local APIs it reads
//! through socket files, such as `tailscaled`'s.

mod build;
mod credential;
pub mod directories;
pub mod dns;
mod listeners;
pub mod runtime_dir;
pub mod tailscale;
mod telemetry;
mod tunnel;
pub mod unix_http;
mod view;
pub use build::BuildConfig;
use credential::Resolution;
pub use credential::{Credential, CredentialError, CredentialFile};
pub use listeners::ListenError;
pub use piqueld_core::{BuildHistoryConfig, ImagesConfig, RetentionConfig};
pub use telemetry::{Headers, HeadersError, OtlpEndpoint, TelemetryConfig};
pub use tunnel::{TunnelConfig, TunnelConfigError, TunnelCredentials};

use piqueld_core::TomlDiagnostic;
use piqueld_core::manifest::PreviewLimits;
use piqueld_core::names::{DNS_LABEL, is_label};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use thiserror::Error;
use utoipa::ToSchema;

/// Implements [`ToSchema`] for `#[serde(try_from = "Raw")]` types as the
/// schema of `Raw`, the TOML form they are read from.
macro_rules! schema_from_raw {
    ($($type:ident => $raw:ty),* $(,)?) => {$(
        impl utoipa::PartialSchema for $type {
            fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                <$raw as utoipa::PartialSchema>::schema()
            }
        }

        impl utoipa::ToSchema for $type {
            fn name() -> std::borrow::Cow<'static, str> {
                stringify!($type).into()
            }

            fn schemas(
                schemas: &mut Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
            ) {
                <$raw as utoipa::ToSchema>::schemas(schemas);
            }
        }
    )*};
}
pub(crate) use schema_from_raw;

/// The daemon's configuration file, `/etc/piqueld/config.toml` unless
/// `piqueld --config` selects another. Every setting is optional unless it
/// says otherwise. The daemon reads the file once at startup, so restart it
/// to apply a change.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// API listeners and state directories.
    #[serde(default)]
    pub server: ServerConfig,
    /// The website's origin and API token lifetimes.
    #[serde(default)]
    pub auth: AuthConfig,
    /// The daemon's own tailnet node, which serves the website over HTTPS.
    #[serde(default)]
    pub tailscale: TailscaleConfig,
    /// The Docker Engine piqueld manages.
    #[serde(default)]
    pub docker: DockerConfig,
    /// Where images build, and how many at once.
    #[serde(default)]
    pub build: BuildConfig,
    /// The managed HTTP gateway that serves applications' routes.
    #[serde(default)]
    pub ingress: IngressConfig,
    /// DNS provider accounts, for certificates and routes' DNS records.
    #[serde(default)]
    pub dns: DnsConfig,
    /// How often piqueld checks for drift, and how long operations may take.
    #[serde(default)]
    pub reconciliation: ReconciliationConfig,
    /// How long finished operations, events and the audit trail are kept.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// How much build output is kept, and for how long.
    #[serde(default)]
    pub build_history: BuildHistoryConfig,
    /// Cleanup of the images this installation builds.
    #[serde(default)]
    pub images: ImagesConfig,
    /// Export of the daemon's traces, metrics and logs over OTLP.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// How many previews may exist, and the resources their services get.
    #[serde(default)]
    pub previews: PreviewLimits,
    /// Uploads of local worktrees by `piquelctl preview deploy --local`.
    #[serde(default)]
    pub snapshots: SnapshotsConfig,
    /// The disk space Git checkouts may take.
    #[serde(default)]
    pub checkouts: CheckoutsConfig,
}

impl DaemonConfig {
    /// The standalone JSON Schema of the configuration file, published as
    /// `docs/piqueld-config.schema.json` for editor completion and inline
    /// errors. It covers structure only: cross-field rules and ranges are
    /// checked when the configuration loads.
    #[must_use]
    pub fn json_schema() -> Value {
        piqueld_core::json_schema::standalone::<Self>("piqueld daemon configuration")
    }

    /// Returns the built-in configuration after applying the same validation
    /// used for file-backed configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if a built-in default violates a
    /// configuration invariant.
    pub fn validated_default() -> Result<Self, ConfigError> {
        let config = Self::default();
        config.validate()?;
        Ok(config)
    }

    /// Reads, parses, and validates configuration without modifying its source.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or the configuration
    /// is malformed or invalid.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::from_toml(&source)
    }

    /// Checks a configuration file as [`Self::load`] would, without reading
    /// credential files or touching anything else on the host. This is
    /// `piqueld check-config`, which can run where the credentials are absent,
    /// such as a Nix build. Credential files are only located: their contents
    /// (readable, non-empty, a tunnel credentials file, a valid webhook URL)
    /// are checked at startup.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or the configuration
    /// is malformed or invalid.
    pub fn check(path: impl AsRef<Path>) -> Result<(), ConfigError> {
        Resolution::Structural.during(|| Self::load(path)).map(drop)
    }

    /// Parses and validates a TOML configuration document. An unset
    /// `auth.public_url` becomes the dashboard hostname's HTTPS origin.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the document is malformed or violates a
    /// host configuration invariant.
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(source)
            .map_err(|error| ConfigError::Parse(TomlDiagnostic::new(source, &error)))?;
        if config.auth.public_url.is_none() {
            config.auth.public_url = config
                .ingress
                .private
                .dashboard_hostname
                .as_ref()
                .map(|hostname| format!("https://{hostname}"));
        }
        config.validate()?;
        Ok(config)
    }

    /// Checks cross-field and range invariants that serde cannot express:
    /// non-zero ports, distinct absolute state directories, a data directory
    /// Docker can bind-mount,
    /// the public origin, the Docker socket path, `server.allowed_hosts` DNS
    /// hostname syntax, timeout and build-log bounds, and preview bounds a
    /// manifest could set itself.
    fn validate(&self) -> Result<(), ConfigError> {
        /// The range of every setting counted in seconds.
        const SECONDS: std::ops::RangeInclusive<u64> = 1..=86_400;
        self.telemetry.validate()?;
        self.previews
            .validate()
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        for (name, path) in [
            ("server.data_dir", &self.server.data_dir),
            ("server.runtime_dir", &self.server.runtime_dir),
        ] {
            absolute_path(name, path)?;
            ConfigError::require(
                path.file_name().is_some(),
                format!("{name} must name a directory"),
            )?;
        }
        ConfigError::require(
            !self.server.data_dir.to_string_lossy().contains([':', ',']),
            "server.data_dir must not contain ':' or ','",
        )?;
        ConfigError::require(
            self.server.data_dir != self.server.runtime_dir,
            "server.data_dir and server.runtime_dir must be different directories",
        )?;
        if let Some(public_url) = &self.auth.public_url {
            AuthConfig::validate_origin(public_url)
                .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        }
        ConfigError::require(
            self.auth.max_token_days != Some(0),
            "auth.max_token_days must be at least 1; omit it to allow tokens without expiry",
        )?;
        self.tailscale.validate()?;
        self.ingress.validate()?;
        ConfigError::require(
            !self.tailscale.enabled
                || !self.ingress.private.enabled
                || self.tailscale.hostname != self.ingress.private.hostname,
            "ingress.private.hostname must differ from tailscale.hostname",
        )?;
        absolute_file("docker.socket", &self.docker.socket)?;
        ConfigError::require(
            self.server.allowed_hosts.iter().all(|host| {
                host.len() <= 253 && host.split('.').all(|label| is_label(label, DNS_LABEL))
            }),
            "server.allowed_hosts must contain DNS hostnames without schemes, ports, or wildcards",
        )?;
        ConfigError::require(
            self.server.port != 0,
            "server.port must be greater than zero",
        )?;
        let reconciliation = &self.reconciliation;
        ConfigError::require(
            SECONDS.contains(&reconciliation.scan_interval_seconds),
            "reconciliation interval must be 1..=86400 seconds",
        )?;
        ConfigError::require(
            SECONDS.contains(&reconciliation.prepare_timeout_seconds)
                && SECONDS.contains(&reconciliation.convergence_timeout_seconds),
            "reconciliation timeouts must be 1..=86400 seconds",
        )?;
        ConfigError::require(
            SECONDS.contains(&self.images.cleanup_interval_seconds),
            "images.cleanup_interval_seconds must be 1..=86400 seconds",
        )?;
        ConfigError::require(
            (1..=64 * 1024 * 1024).contains(&self.build_history.log_max_bytes)
                && (1..=3650).contains(&self.build_history.log_retention_days),
            "build logs require 1..=67108864 bytes and 1..=3650 retention days",
        )
    }

    /// Effective website origin. The dashboard hostname fills an unset value
    /// at load, and otherwise a tailnet node does when it joins, before
    /// authentication starts.
    #[must_use]
    pub fn public_url(&self) -> &str {
        self.auth
            .public_url
            .as_deref()
            .unwrap_or("http://localhost:7845")
    }
}

/// The website's origin and API token lifetimes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// The website's canonical origin, such as `https://piqueld.example.com`.
    /// Passkeys only work on this origin, and API mutations and WebSocket
    /// upgrades that send an `Origin` header must match it. It must be HTTPS,
    /// except `http://localhost` or a subdomain of it, such as
    /// `http://dev.localhost`, for development. When unset, it is
    /// `https://` followed by `ingress.private.dashboard_hostname` if that is
    /// set, otherwise the tailnet node's HTTPS URL if `tailscale.enabled` is
    /// set, otherwise `http://localhost:7845`.
    pub public_url: Option<String>,
    /// The longest lifetime, in days, of new API tokens. When unset, tokens
    /// may have any lifetime, including none. Existing tokens keep theirs.
    #[schema(minimum = 1)]
    pub max_token_days: Option<u32>,
}

impl AuthConfig {
    /// Validates the canonical origin used for `WebAuthn`, links, and CSRF checks.
    ///
    /// The URL must be a bare origin with a DNS hostname: no credentials, path,
    /// query, or fragment.
    ///
    /// ```text
    /// https://piqueld.example.com   accepted
    /// http://localhost:8080         accepted (development)
    /// http://dev.localhost:8080     accepted (development)
    /// https://10.0.0.1              rejected (no DNS hostname)
    /// https://example.com/piqueld   rejected (path)
    /// ```
    /// # Errors
    /// Requires HTTPS, except for HTTP development on `localhost` or its
    /// subdomains, which browsers resolve to loopback and treat as secure.
    #[expect(
        clippy::map_err_ignore,
        reason = "the error is a fixed message that already names the setting"
    )]
    pub fn validate_origin(value: &str) -> Result<url::Url, InvalidOrigin> {
        let url = url::Url::parse(value).map_err(|_| InvalidOrigin("invalid public_url"))?;
        if url.domain().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http"
                    && url.domain().is_some_and(piqueld_core::auth::is_localhost)))
        {
            return Err(InvalidOrigin(
                "public_url must be an HTTPS origin (or http://localhost or a subdomain of it for development)",
            ));
        }
        Ok(url)
    }
}

/// Why a `public_url` can't be the website's origin.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("{0}")]
pub struct InvalidOrigin(pub &'static str);

/// The daemon's own tailnet node, which serves the website (dashboard and
/// API) over HTTPS with a tailnet certificate. The tailnet needs `MagicDNS`
/// and HTTPS certificates enabled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TailscaleConfig {
    /// Join the tailnet as a node and serve the website on its `https_port`.
    /// The `tailscale` CLI must be on `PATH`, since piqueld runs it to log
    /// the node in and change its Serve configuration. So must `tailscaled`
    /// unless `socket` is set, since piqueld otherwise starts its own.
    pub enabled: bool,
    /// The node's name, a single DNS label, which becomes
    /// `<hostname>.<tailnet>.ts.net`. Unused with `socket`, whose node
    /// already has a name.
    pub hostname: String,
    /// A Tailscale auth key for the node's first login. Without one, the
    /// daemon logs a login URL and waits until the node is approved. Later
    /// starts reuse the node's state in the data directory. piqueld passes
    /// the file to Tailscale on every start without reading it, so it must
    /// exist at every start; its key is only used for the first login. Not
    /// allowed with `socket`.
    pub auth_key_file: Option<CredentialFile>,
    /// The absolute path of the socket of a logged-in `tailscaled` to share
    /// instead of starting one. piqueld then only adds `https_port` to that
    /// node's Serve configuration, so several daemons can share one node,
    /// each on its own port. That `tailscaled` must run as the daemon's user.
    #[schema(value_type = Option<String>)]
    pub socket: Option<PathBuf>,
    /// The node port serving the website over HTTPS.
    #[schema(value_type = u16, minimum = 1, maximum = 65_535)]
    pub https_port: NonZeroU16,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "piqueld".into(),
            auth_key_file: None,
            socket: None,
            https_port: const { NonZeroU16::new(443).expect("443 is not zero") },
        }
    }
}

impl TailscaleConfig {
    /// Checks the node name, and the socket and auth key settings against each other.
    fn validate(&self) -> Result<(), ConfigError> {
        dns_label("tailscale.hostname", &self.hostname)?;
        if let Some(socket) = &self.socket {
            absolute_file("tailscale.socket", socket)?;
            ConfigError::require(
                self.auth_key_file.is_none(),
                "tailscale.auth_key_file is only used by piqueld's own tailscaled; \
                 log the shared tailscaled in instead",
            )?;
        }
        Ok(())
    }
}

/// Requires a tailnet node name: a single DNS label.
fn dns_label(name: &str, value: &str) -> Result<(), ConfigError> {
    ConfigError::require(
        is_label(value, DNS_LABEL),
        format!("{name} must be a single DNS label"),
    )
}

/// The managed Caddy gateway that serves applications' routes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngressConfig {
    /// Run the managed gateway and serve public routes on ports 80 and 443,
    /// or through `tunnel`. Requires Docker Engine 28 or later. Disabling it
    /// removes the gateway but keeps routes and certificates.
    #[serde(default)]
    #[schema(default = false)]
    pub enabled: bool,
    /// This server's public IPv4 and IPv6 addresses, such as
    /// `["203.0.113.10", "2001:db8::10"]`. Public routes in zones whose DNS
    /// provider sets `manage_records` get A and AAAA records to them; without
    /// any, those records stay manual. Unused with `tunnel`.
    #[serde(default)]
    #[schema(value_type = Vec<String>, default = json!([]))]
    pub public_addresses: Vec<std::net::IpAddr>,
    /// The ACME account DNS-01 certificates are ordered with.
    #[serde(default)]
    pub acme: AcmeConfig,
    /// The apps tailnet node, which serves private routes.
    #[serde(default)]
    pub private: PrivateIngressConfig,
    /// The Cloudflare Tunnel that replaces ports 80 and 443 for public routes.
    #[serde(default)]
    pub tunnel: TunnelConfig,
    /// A public hostname that serves GitHub push webhooks, and nothing else,
    /// for applications whose repository syncs with `webhook`, while
    /// `ingress.enabled` is set. No application may route it or its
    /// subdomains.
    #[serde(default)]
    pub webhook_hostname: Option<piqueld_core::manifest::Hostname>,
    /// Count the requests each route serves, for each environment's
    /// traffic page, while `ingress.enabled` is set. piqueld stores hourly
    /// totals, not the requests themselves or visitors' addresses. While the
    /// daemon isn't connected to the gateway, such as while it restarts, the
    /// gateway writes its requests, visitors' addresses and paths included, to
    /// its own Docker log, which keeps up to 30 MB.
    #[serde(default = "enabled")]
    #[schema(default = true)]
    pub traffic: bool,
}

/// `true`, the default of settings on unless turned off.
const fn enabled() -> bool {
    true
}

impl Default for IngressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            public_addresses: Vec::new(),
            acme: AcmeConfig::default(),
            private: PrivateIngressConfig::default(),
            tunnel: TunnelConfig::default(),
            webhook_hostname: None,
            traffic: enabled(),
        }
    }
}

impl IngressConfig {
    /// Checks the ACME account, the apps node's name, and that the dashboard
    /// hostname has private ingress to serve it and differs from the webhook
    /// hostname.
    fn validate(&self) -> Result<(), ConfigError> {
        self.acme.validate()?;
        dns_label("ingress.private.hostname", &self.private.hostname)?;
        if let Some(dashboard) = &self.private.dashboard_hostname {
            ConfigError::require(
                self.enabled && self.private.enabled,
                "ingress.private.dashboard_hostname requires ingress.enabled and ingress.private.enabled",
            )?;
            ConfigError::require(
                self.webhook_hostname.as_ref() != Some(dashboard),
                "ingress.private.dashboard_hostname must differ from ingress.webhook_hostname",
            )?;
        }
        Ok(())
    }
}

/// The `[ingress.private]` settings, with the auth key read. Its schema, and
/// its documentation, are those of its TOML form, `RawPrivateIngressConfig`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawPrivateIngressConfig")]
pub struct PrivateIngressConfig {
    /// Run the node and serve private routes on the tailnet.
    pub enabled: bool,
    /// Node name, which becomes `<hostname>.<tailnet>.ts.net`.
    pub hostname: String,
    /// Auth key for the first login, read from `auth_key_file`.
    pub auth_key: Option<Credential>,
    /// Private hostname serving the website to the tailnet. The private
    /// listener forwards it to a socket in the gateway's control mount,
    /// which the daemon serves.
    pub dashboard_hostname: Option<piqueld_core::manifest::Hostname>,
}

impl Default for PrivateIngressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "piqueld-apps".into(),
            auth_key: None,
            dashboard_hostname: None,
        }
    }
}

/// The apps tailnet node: a second tailnet node, a Tailscale container
/// beside the gateway, that carries private routes' traffic to the
/// gateway's private listener. It is separate from the daemon's own node
/// (`[tailscale]`), so it can have its own ACLs and tags.
#[derive(Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
struct RawPrivateIngressConfig {
    /// Run the apps node and serve private routes on the tailnet. While it is
    /// disabled, private routes report `disabled`. Private routes get DNS-01
    /// certificates, so their zones need a `[[dns.providers]]` entry.
    enabled: bool,
    /// The apps node's name, a single DNS label, which becomes
    /// `<hostname>.<tailnet>.ts.net`. It must differ from
    /// `tailscale.hostname` while both are enabled.
    hostname: String,
    /// A Tailscale auth key for the apps node's first login. Without one,
    /// the daemon logs the node's login URL.
    auth_key_file: Option<CredentialFile>,
    /// A private hostname, such as `piqueld.example.com`, on which the
    /// gateway serves the website (dashboard and API) to the tailnet, with a
    /// DNS-01 certificate. An unset `auth.public_url` becomes its HTTPS
    /// origin. No application may route it or its subdomains. Requires
    /// `ingress.enabled` and `ingress.private.enabled`, and must differ from
    /// `ingress.webhook_hostname`.
    dashboard_hostname: Option<piqueld_core::manifest::Hostname>,
}

impl Default for RawPrivateIngressConfig {
    fn default() -> Self {
        let defaults = PrivateIngressConfig::default();
        Self {
            enabled: defaults.enabled,
            hostname: defaults.hostname,
            auth_key_file: None,
            dashboard_hostname: defaults.dashboard_hostname,
        }
    }
}

impl TryFrom<RawPrivateIngressConfig> for PrivateIngressConfig {
    type Error = CredentialError;
    fn try_from(raw: RawPrivateIngressConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            enabled: raw.enabled,
            hostname: raw.hostname,
            auth_key: raw
                .auth_key_file
                .map(|file| Credential::read("auth_key", file))
                .transpose()?,
            dashboard_hostname: raw.dashboard_hostname,
        })
    }
}

schema_from_raw!(PrivateIngressConfig => RawPrivateIngressConfig);

/// The certificate authority (CA) piqueld orders DNS-01 certificates from,
/// for hostnames a public CA cannot reach over HTTP, such as those of
/// private routes. The gateway obtains other certificates itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    /// The ACME directory URL of the CA, which must be HTTPS. The default is
    /// Let's Encrypt's production CA.
    pub directory: String,
    /// An email address the CA may send expiry and policy notices to.
    /// piqueld sends it only when it first registers its account with the CA.
    pub email: Option<String>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            email: None,
        }
    }
}

impl AcmeConfig {
    /// Requires an HTTPS directory URL and an email address without whitespace.
    fn validate(&self) -> Result<(), ConfigError> {
        ConfigError::require(
            url::Url::parse(&self.directory).is_ok_and(|url| url.scheme() == "https"),
            "ingress.acme.directory must be an HTTPS URL",
        )?;
        ConfigError::require(
            self.email.as_ref().is_none_or(|email| {
                email
                    .split_once('@')
                    .is_some_and(|(user, domain)| !user.is_empty() && !domain.is_empty())
                    && !email.contains(char::is_whitespace)
            }),
            "ingress.acme.email must be an email address",
        )
    }
}

/// DNS provider accounts. piqueld uses them to obtain DNS-01 certificates
/// and, where `manage_records` is set, to maintain routes' DNS records. It
/// discovers each account's zones through the provider's API at startup and
/// hourly; a hostname belongs to the provider with the longest matching
/// zone.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsConfig {
    /// DNS provider accounts, one `[[dns.providers]]` table each, selected by
    /// `kind`.
    #[serde(default)]
    #[schema(default = json!([]))]
    pub providers: Vec<dns::provider::DnsProviderConfig>,
}

/// Uploads of local worktrees by `piquelctl preview deploy --local`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotsConfig {
    /// The largest Git bundle one upload may carry, in bytes. Larger uploads
    /// fail with `snapshot_too_large`.
    #[schema(value_type = u64, minimum = 1)]
    pub max_upload_bytes: std::num::NonZeroU64,
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: const {
                std::num::NonZeroU64::new(256 * 1024 * 1024)
                    .expect("the default upload limit is not zero")
            },
        }
    }
}

/// The Git checkouts deployments read manifests from and build sources in,
/// under `<data_dir>/checkouts`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CheckoutsConfig {
    /// The most bytes all checkouts, history and working tree, may take at
    /// once. A checkout waits until it fits beside the others, and one larger
    /// than this, or a local snapshot that would be, is refused.
    #[schema(value_type = u64, minimum = 1)]
    pub max_bytes: std::num::NonZeroU64,
}

impl Default for CheckoutsConfig {
    fn default() -> Self {
        Self {
            max_bytes: const {
                std::num::NonZeroU64::new(4 * 1024 * 1024 * 1024)
                    .expect("the default checkout budget is not zero")
            },
        }
    }
}

/// Rejects relative paths, naming the offending setting in the error.
fn absolute_path(name: &str, path: &Path) -> Result<(), ConfigError> {
    ConfigError::require(
        path.is_absolute(),
        format!("{name} must be an absolute path"),
    )
}

/// Requires an absolute path that ends in a file name (not `/` or `..`).
fn absolute_file(name: &str, path: &Path) -> Result<(), ConfigError> {
    absolute_path(name, path)?;
    ConfigError::require(
        path.file_name().is_some(),
        format!("{name} must name a file"),
    )
}

/// Where the daemon keeps its state and serves its API. The Unix socket
/// `<runtime_dir>/piqueld.sock` always serves the API; `listen_mode` adds
/// TCP listeners, which also serve the dashboard.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// The absolute path of the private directory holding the database,
    /// `secrets.key` and the rest of the daemon's state. Missing directories
    /// are created with mode `0700`. It can't contain `:` or `,`, which
    /// Docker would read as separators when piqueld mounts it into the
    /// ingress containers.
    #[schema(value_type = String)]
    pub data_dir: PathBuf,
    /// The absolute path of the directory holding the Unix API socket,
    /// `piqueld.sock`. It must already exist, owned by the daemon's user and
    /// with full access for it. Its group may read or traverse it, as with
    /// mode `0750`, `0740` or `0710`, only when it is the daemon's effective
    /// group; otherwise use `0700`. Group write and any access by others are
    /// refused. Every directory above it must be owned by root or the
    /// daemon's user and writable only by its owner, unless sticky, like
    /// `/tmp`. It must differ from `data_dir`.
    #[schema(value_type = String)]
    pub runtime_dir: PathBuf,
    /// The TCP addresses that serve the API and dashboard over plain HTTP,
    /// besides the Unix socket. Every request must still authenticate.
    ///
    /// - `off`: none, so only the Unix socket serves the API.
    /// - `localhost`: IPv4 and IPv6 loopback, `127.0.0.1` and `::1`.
    /// - `tailscale`: the host's Tailscale addresses, read from its
    ///   `tailscaled` at startup. If Tailscale isn't running then,
    ///   piqueld logs a warning and tries again only after a restart.
    /// - `both`: loopback and Tailscale addresses.
    pub listen_mode: ListenMode,
    /// DNS hostnames, such as `my-host.my-tailnet.ts.net`, that TCP requests
    /// may use besides `localhost` and IP addresses. Write exact hostnames,
    /// without schemes, ports or wildcards, and only names trusted
    /// operators control.
    pub allowed_hosts: Vec<String>,
    /// The port every TCP listener uses.
    #[schema(minimum = 1, maximum = 65_535)]
    pub port: u16,
}

/// Where the API and dashboard listen over TCP.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListenMode {
    /// The Unix socket only.
    #[default]
    Off,
    /// IPv4 and IPv6 loopback.
    Localhost,
    /// The host's Tailscale addresses, discovered at startup.
    Tailscale,
    /// Loopback and Tailscale addresses.
    Both,
}

impl std::fmt::Display for ListenMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Localhost => "localhost",
            Self::Tailscale => "tailscale",
            Self::Both => "both",
        })
    }
}

impl ServerConfig {
    /// Unix API socket path inside the runtime directory.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join(runtime_dir::SOCKET_NAME)
    }

    /// Embedded database path inside the data directory.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("piqueld.db")
    }

    /// Private tailnet node state inside the data directory, so the node
    /// identity (and the passkeys bound to its name) follows the data.
    #[must_use]
    pub fn tailscale_dir(&self) -> PathBuf {
        self.data_dir.join("tailscale")
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/piqueld"),
            runtime_dir: PathBuf::from("/run/piqueld"),
            listen_mode: ListenMode::default(),
            port: 7845,
            allowed_hosts: Vec::new(),
        }
    }
}

/// The Docker Engine piqueld deploys to. The daemon
/// serves its API while Docker is down, and starts deploying once Docker
/// answers as a single-node Swarm manager.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    /// The absolute path of the Docker Engine's Unix socket.
    #[schema(value_type = String)]
    pub socket: PathBuf,
    /// Initialize a Docker Engine that is not part of a Swarm as a
    /// single-node Swarm.
    pub auto_initialize_swarm: bool,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/docker.sock"),
            auto_initialize_swarm: true,
        }
    }
}

/// How often piqueld checks that Docker matches what is deployed, and how
/// long a deployment may take.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReconciliationConfig {
    /// Seconds between full scans for drift between Docker and what is
    /// deployed.
    #[schema(minimum = 1, maximum = 86_400)]
    pub scan_interval_seconds: u64,
    /// Seconds a deployment may take to prepare: fetching its manifest,
    /// resolving images and building them, including the time spent waiting
    /// for a build slot.
    #[schema(minimum = 1, maximum = 86_400)]
    pub prepare_timeout_seconds: u64,
    /// Seconds a deployment may wait without any service converging. It
    /// restarts each time a service converges, so each link of a startup
    /// dependency chain gets the full timeout.
    #[schema(minimum = 1, maximum = 86_400)]
    pub convergence_timeout_seconds: u64,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 60,
            prepare_timeout_seconds: 300,
            convergence_timeout_seconds: 120,
        }
    }
}

/// Configuration loading or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the source file failed.
    #[error("could not read configuration")]
    Read(#[source] std::io::Error),
    /// The TOML document was syntactically malformed, had the wrong shape,
    /// contained unknown keys, or named an unreadable credential file. Never
    /// carries configuration source text.
    #[error("configuration could not be parsed")]
    Parse(#[source] TomlDiagnostic),
    /// A parsed setting violated a semantic invariant.
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

impl ConfigError {
    /// A configuration check: fails with [`Self::Invalid`] and `message`
    /// unless `valid`.
    fn require(valid: bool, message: impl Into<String>) -> Result<(), Self> {
        if valid {
            Ok(())
        } else {
            Err(Self::Invalid(message.into()))
        }
    }
}

#[cfg(test)]
mod tests;
