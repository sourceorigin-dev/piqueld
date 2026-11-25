//! Queries a zone's authoritative nameservers directly over TCP without
//! caching: to wait for a published TXT record, so a CA resolving the name
//! afterwards finds it (OVH can take several minutes to propagate changes),
//! and to resolve a route's hostname for its check, so a stale answer cached
//! on this host cannot hide records piqueld published since.
use super::{Dns, ZoneError};
use hickory_resolver::{
    Resolver, TokioResolver,
    config::{ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts},
    net::NetError,
    net::runtime::TokioRuntimeProvider,
    proto::rr::{Name, RData},
};
use piqueld_core::manifest::Hostname;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use thiserror::Error;

/// Why a record did not propagate or a hostname could not be resolved on a
/// zone's authoritative nameservers.
#[derive(Debug, Error)]
pub enum PropagationError {
    /// The record stayed invisible until the timeout.
    #[error(
        "TXT record {name} was not visible on every authoritative nameserver of {zone} within {seconds}s"
    )]
    Timeout {
        /// The record's name.
        name: String,
        /// The zone.
        zone: Hostname,
        /// The timeout, in seconds.
        seconds: u64,
        /// The elapsed timer.
        #[source]
        source: tokio::time::error::Elapsed,
    },
    /// No provider zone contains the hostname.
    #[error(transparent)]
    Zone(#[from] ZoneError),
    /// The resolver could not be built.
    #[error(transparent)]
    Resolver(Box<NetError>),
    /// The zone's NS records could not be looked up.
    #[error("look up NS records of {zone}")]
    NsLookup {
        /// The zone.
        zone: Hostname,
        /// The lookup failure.
        #[source]
        source: Box<NetError>,
    },
    /// A nameserver's address could not be looked up.
    #[error("resolve nameserver {nameserver}")]
    NameserverLookup {
        /// The nameserver's name.
        nameserver: Name,
        /// The lookup failure.
        #[source]
        source: Box<NetError>,
    },
    /// The zone has no NS record.
    #[error("{0} has no authoritative nameservers")]
    NoNameservers(Hostname),
    /// A nameserver resolved to no address.
    #[error("a nameserver of {0} has no address")]
    NoAddress(Hostname),
    /// A nameserver did not answer the TXT query.
    #[error("query {address}")]
    Query {
        /// The nameserver's address.
        address: SocketAddr,
        /// The query failure.
        #[source]
        source: Box<NetError>,
    },
    /// The hostname could not be resolved on the zone's nameservers.
    #[error("resolve {hostname} on the nameservers of {zone}")]
    Resolve {
        /// The hostname.
        hostname: Hostname,
        /// The zone.
        zone: Hostname,
        /// The lookup failure.
        #[source]
        source: Box<NetError>,
    },
}

impl From<NetError> for PropagationError {
    fn from(error: NetError) -> Self {
        Self::Resolver(Box::new(error))
    }
}

impl Dns {
    /// Polls every 5s until every authoritative nameserver of `zone` serves
    /// `value` in a TXT record at `name`, failing after `timeout`.
    ///
    /// # Errors
    /// Fails when the record is not visible everywhere in time.
    pub async fn wait_visible(
        &self,
        zone: &Hostname,
        name: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<(), PropagationError> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.visible(zone, name, value).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::debug!(%zone, name, error = format!("{error:#}"), "TXT propagation check failed");
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
        .await
        .map_err(|source| PropagationError::Timeout {
            name: name.to_owned(),
            zone: zone.clone(),
            seconds: timeout.as_secs(),
            source,
        })
    }

    /// Whether every authoritative nameserver of `zone` already serves `value`.
    /// A nameserver answers through the first of its addresses that responds,
    /// so an unreachable IPv6 address does not hide a working IPv4 one.
    async fn visible(
        &self,
        zone: &Hostname,
        name: &str,
        value: &str,
    ) -> Result<bool, PropagationError> {
        'nameservers: for addresses in self.nameservers(zone).await? {
            let mut failure = None;
            for address in addresses {
                match Self::serves(address, name, value).await {
                    Ok(true) => continue 'nameservers,
                    Ok(false) => return Ok(false),
                    Err(error) => failure = Some(error),
                }
            }
            return Err(failure.unwrap_or_else(|| PropagationError::NoAddress(zone.clone())));
        }
        Ok(true)
    }

    /// Whether the nameserver at `address` serves `value` in a TXT record at
    /// `name`.
    async fn serves(
        address: SocketAddr,
        name: &str,
        value: &str,
    ) -> Result<bool, PropagationError> {
        match Self::authoritative([address])?
            .txt_lookup(format!("{name}."))
            .await
        {
            Ok(lookup) => Ok(lookup.answers().iter().any(|record| {
                matches!(&record.data, RData::TXT(txt)
                    if txt.txt_data.iter().any(|part| &**part == value.as_bytes()))
            })),
            Err(error) if error.is_no_records_found() => Ok(false),
            Err(source) => Err(PropagationError::Query {
                address,
                source: Box::new(source),
            }),
        }
    }

    /// Addresses `hostname` has on the authoritative nameservers of the zone
    /// owning it, so a route check sees the records piqueld wrote rather than
    /// an answer this host cached before.
    ///
    /// # Errors
    /// Fails when no provider zone contains `hostname`, or the lookup fails or
    /// finds no address.
    pub async fn resolve_authoritatively(
        &self,
        hostname: &Hostname,
    ) -> Result<BTreeSet<IpAddr>, PropagationError> {
        let (_, zone) = self.zone_for(hostname).await?;
        let nameservers = self.nameservers(&zone.name).await?.into_iter().flatten();
        Ok(Self::authoritative(nameservers)?
            .lookup_ip(format!("{hostname}."))
            .await
            .map_err(|source| PropagationError::Resolve {
                hostname: hostname.clone(),
                zone: zone.name.clone(),
                source: Box::new(source),
            })?
            .iter()
            .collect())
    }

    /// A resolver asking the nameservers at `addresses` over TCP, without
    /// recursion, caching or the hosts file. It moves on to the next address
    /// when one does not answer.
    fn authoritative(
        addresses: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<TokioResolver, PropagationError> {
        let nameservers = addresses
            .into_iter()
            .map(|address| {
                let mut connection = ConnectionConfig::tcp();
                connection.port = address.port();
                NameServerConfig::new(address.ip(), true, vec![connection])
            })
            .collect();
        let mut options = ResolverOpts::default();
        options.cache_size = 0;
        options.recursion_desired = false;
        options.use_hosts_file = ResolveHosts::Never;
        Ok(Resolver::builder_with_config(
            ResolverConfig::from_parts(None, Vec::new(), nameservers),
            TokioRuntimeProvider::default(),
        )
        .with_options(options)
        .build()?)
    }

    /// Addresses of each authoritative nameserver of `zone`, from its NS
    /// records.
    async fn nameservers(&self, zone: &Hostname) -> Result<Vec<Vec<SocketAddr>>, PropagationError> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(nameservers) = &self.nameservers {
            return Ok(nameservers.clone());
        }
        let system = TokioResolver::builder_tokio()?.build()?;
        let records = system
            .ns_lookup(format!("{zone}."))
            .await
            .map_err(|source| PropagationError::NsLookup {
                zone: zone.clone(),
                source: Box::new(source),
            })?;
        let mut nameservers = Vec::new();
        for record in records.answers() {
            if let RData::NS(nameserver) = &record.data {
                let ips = system
                    .lookup_ip(nameserver.0.clone())
                    .await
                    .map_err(|source| PropagationError::NameserverLookup {
                        nameserver: nameserver.0.clone(),
                        source: Box::new(source),
                    })?;
                nameservers.push(ips.iter().map(|ip| SocketAddr::new(ip, 53)).collect());
            }
        }
        if nameservers.is_empty() {
            return Err(PropagationError::NoNameservers(zone.clone()));
        }
        Ok(nameservers)
    }
}
