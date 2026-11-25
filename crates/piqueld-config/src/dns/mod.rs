//! DNS across the configured providers (`[[dns.providers]]` in daemon TOML).
//!
//! piqueld calls provider APIs itself, so DNS credentials never enter a
//! container. Providers publish ACME DNS-01 TXT records and, when
//! `manage_records` is set, routes' A/AAAA/CNAME records; [`provider`] holds
//! the provider abstraction and each provider's API.
//!
//! [`Dns`] discovers each provider's zones and assigns every hostname to the
//! provider with the longest matching zone. It also waits for records to
//! propagate to a zone's nameservers, and resolves route hostnames on them.
mod propagation;
pub mod provider;
mod zones;

pub use propagation::PropagationError;
pub use zones::ZoneError;

use piqueld_core::{api::DnsProviderStatus, manifest::Hostname};
use provider::{DnsProvider, DnsProviderConfig, Zone};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use zones::Zones;

/// The latest zone discovery of one provider.
#[derive(Default)]
struct Discovery {
    /// Zones from the last successful discovery; a failed refresh keeps them.
    zones: Vec<Zone>,
    /// The latest discovery failure, cleared by a success.
    error: Option<String>,
    /// Failed discoveries since the last success, which lengthen the wait
    /// before the next.
    failures: u32,
    /// When [`Dns::refresh`] discovers this provider's zones again; `None`
    /// until the first discovery.
    due: Option<Instant>,
}

impl Discovery {
    /// Zones are rediscovered this long after a success.
    const REFRESH: Duration = Duration::from_hours(1);
    /// The wait after a first failure, doubling with each further one.
    const RETRY: Duration = Duration::from_mins(1);
    /// The longest wait after failures.
    const MAX_RETRY: Duration = Duration::from_mins(15);

    /// Whether [`Dns::refresh`] should discover this provider's zones at `now`.
    fn is_due(&self, now: Instant) -> bool {
        self.due.is_none_or(|due| due <= now)
    }

    /// Records a discovery that ended at `now`. A failure keeps the old
    /// zones and backs off before the next attempt.
    fn record(&mut self, result: Result<Vec<Zone>, provider::DnsError>, now: Instant) {
        match result {
            Ok(zones) => {
                *self = Self {
                    zones,
                    due: Some(now + Self::REFRESH),
                    ..Self::default()
                };
            }
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "DNS zone discovery failed");
                self.error = Some(
                    std::iter::successors(Some(&error as &dyn std::error::Error), |error| {
                        error.source()
                    })
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(": "),
                );
                let wait = Self::RETRY.saturating_mul(1 << self.failures.min(4));
                self.failures += 1;
                self.due = Some(now + wait.min(Self::MAX_RETRY));
            }
        }
    }

    /// Every provider's zones, from their `discoveries`.
    fn zones(discoveries: &[Self]) -> Zones<'_> {
        Zones(
            discoveries
                .iter()
                .map(|discovery| discovery.zones.as_slice())
                .collect(),
        )
    }
}

/// Configured providers with their discovered zones.
pub struct Dns {
    providers: Vec<DnsProviderConfig>,
    /// Shared client for provider APIs; never follows redirects or proxies.
    http: reqwest::Client,
    /// One entry per provider, in configuration order.
    discovered: RwLock<Vec<Discovery>>,
    /// Held for a whole discovery, so discoveries never overlap and an older
    /// one cannot overwrite a newer result.
    discovering: Mutex<()>,
    /// Addresses of each nameserver queried for propagation and route
    /// hostnames instead of each zone's NS records.
    #[cfg(any(test, feature = "testing"))]
    pub nameservers: Option<Vec<Vec<std::net::SocketAddr>>>,
}

impl Dns {
    /// Longest a provider API request may take.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Creates the inert integration; zones are discovered by [`Self::refresh`].
    ///
    /// # Errors
    /// Returns HTTP client initialization failures.
    pub fn new(providers: Vec<DnsProviderConfig>) -> reqwest::Result<Self> {
        Ok(Self {
            discovered: RwLock::new(providers.iter().map(|_| Discovery::default()).collect()),
            providers,
            http: reqwest::Client::builder()
                .timeout(Self::REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()?,
            discovering: Mutex::new(()),
            #[cfg(any(test, feature = "testing"))]
            nameservers: None,
        })
    }

    /// The client used for provider APIs.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The configured provider at `index`.
    pub fn provider(&self, index: usize) -> &DnsProvider {
        &self.providers[index].provider
    }

    /// Whether any provider manages routes' records.
    pub fn manages_any_records(&self) -> bool {
        self.providers.iter().any(|config| config.manage_records)
    }

    /// Whether the provider at `index` manages routes' records.
    pub fn manages_records(&self, index: usize) -> bool {
        self.providers[index].manage_records
    }

    /// Rediscovers each provider's zones that are older than an hour, or
    /// whose failed discovery has waited out its backoff: 1 minute, doubling
    /// up to 15.
    pub async fn refresh(&self) {
        self.discover_where(Discovery::is_due).await;
    }

    /// Lists every provider's zones now, which also checks their credentials.
    /// Failures are kept per provider for status, and their old zones stay in
    /// use. Credential files are only read at startup.
    pub async fn discover(&self) {
        self.discover_where(|_, _| true).await;
    }

    /// Discovers the zones of the providers `wanted` selects, one at a time.
    async fn discover_where(&self, wanted: impl Fn(&Discovery, Instant) -> bool) {
        let _discovering = self.discovering.lock().await;
        let now = Instant::now();
        let wanted: Vec<usize> = (self.discovered.read().await.iter().enumerate())
            .filter(|(_, discovery)| wanted(discovery, now))
            .map(|(index, _)| index)
            .collect();
        for index in wanted {
            let result = self.providers[index].provider.zones(&self.http).await;
            self.discovered.write().await[index].record(result, Instant::now());
        }
    }

    /// The provider (by index) and zone that own `hostname`.
    ///
    /// # Errors
    /// No zone contains it, or its longest matching zone has two providers.
    pub async fn zone_for(&self, hostname: &Hostname) -> Result<(usize, Zone), ZoneError> {
        let discovered = self.discovered.read().await;
        Discovery::zones(&discovered)
            .owner(hostname)
            .map(|(index, zone)| (index, zone.clone()))
    }

    /// Per-provider kind, zones and discovery health.
    pub async fn status(&self) -> Vec<DnsProviderStatus> {
        let discovered = self.discovered.read().await;
        let zones = Discovery::zones(&discovered);
        self.providers
            .iter()
            .zip(discovered.iter())
            .enumerate()
            .map(|(index, (config, discovery))| {
                let conflicts = zones.conflicts(index);
                let message = match (&discovery.error, conflicts.is_empty()) {
                    (Some(error), _) => format!("Zone discovery failed: {error}"),
                    (None, false) => format!(
                        "Zones also claimed by another provider are not used: {}",
                        conflicts.join(", ")
                    ),
                    (None, true) if discovery.zones.is_empty() => "No zones discovered yet".into(),
                    (None, true) => "Zones discovered".into(),
                };
                DnsProviderStatus {
                    kind: config.provider.kind().into(),
                    manage_records: config.manage_records,
                    zones: discovery.zones.iter().map(|z| z.name.to_string()).collect(),
                    healthy: discovery.error.is_none() && conflicts.is_empty(),
                    message,
                }
            })
            .collect()
    }
}
