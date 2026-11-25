//! Startup-only TCP listener selection and Tailscale discovery.

use super::{ListenMode, ServerConfig};
use crate::tailscale::{HOST_SOCKET, LocalApi, LocalApiError, Status};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use thiserror::Error;
use tokio::net::TcpListener;

/// Bounds discovery, so a stuck `tailscaled` can't hold startup up.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the daemon's TCP listeners could not be selected or bound.
#[derive(Debug, Error)]
pub enum ListenError {
    /// A configured address could not be bound.
    #[error("failed to bind HTTP API on {address}")]
    Bind {
        /// The address.
        address: SocketAddr,
        /// The bind failure.
        #[source]
        source: std::io::Error,
    },
    /// The host's `tailscaled` status could not be read.
    #[error("failed to read the host's Tailscale status")]
    Status(#[source] LocalApiError),
    /// The node is not logged in and connected.
    #[error("Tailscale is {0}")]
    NotRunning(String),
    /// The node has no tailnet address.
    #[error("Tailscale has no assigned addresses")]
    NoAddresses,
    /// An address is unspecified, loopback or multicast.
    #[error("Tailscale reported an invalid interface address")]
    InvalidAddress,
}

impl ServerConfig {
    /// Binds every configured TCP address before the daemon starts serving.
    /// Unavailable Tailscale is warning-only and is never retried.
    ///
    /// # Errors
    /// Any bind failure aborts startup and releases listeners already bound.
    pub async fn bind_tcp(&self) -> Result<Vec<TcpListener>, ListenError> {
        let tailscale = if matches!(self.listen_mode, ListenMode::Tailscale | ListenMode::Both) {
            let api = LocalApi::new(HOST_SOCKET.into(), DISCOVERY_TIMEOUT);
            match Status::discover(&api).await {
                Ok(addresses) => addresses,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"),
                        "Tailscale listening is enabled but unavailable; restart piqueld to retry");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut listeners = Vec::new();
        for address in self.listen_addresses(tailscale) {
            listeners.push(
                TcpListener::bind(address)
                    .await
                    .map_err(|source| ListenError::Bind { address, source })?,
            );
        }
        Ok(listeners)
    }

    /// Expands the listen mode into concrete socket addresses on the shared port.
    ///
    /// ```text
    /// localhost -> 127.0.0.1:7845, [::1]:7845
    /// tailscale -> <each discovered Tailscale IP>:7845
    /// both      -> loopback addresses followed by Tailscale addresses
    /// ```
    fn listen_addresses(&self, tailscale: Vec<IpAddr>) -> Vec<SocketAddr> {
        let mut addresses = Vec::new();
        if matches!(self.listen_mode, ListenMode::Localhost | ListenMode::Both) {
            addresses.extend([
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]);
        }
        if matches!(self.listen_mode, ListenMode::Tailscale | ListenMode::Both) {
            addresses.extend(tailscale);
        }
        addresses
            .into_iter()
            .map(|ip| SocketAddr::new(ip, self.port))
            .collect()
    }
}

impl Status {
    /// Reads the host's Tailscale addresses from its `tailscaled`.
    async fn discover(api: &LocalApi) -> Result<Vec<IpAddr>, ListenError> {
        api.status()
            .await
            .map_err(ListenError::Status)?
            .listen_addresses()
    }

    /// Extracts sorted, deduplicated addresses, requiring a `Running` backend and a
    /// non-empty list in which every address is concrete (not unspecified,
    /// loopback, or multicast).
    fn listen_addresses(&self) -> Result<Vec<IpAddr>, ListenError> {
        if !self.running() {
            return Err(ListenError::NotRunning(self.backend_state.clone()));
        }
        let mut addresses: Vec<IpAddr> = self.addresses().collect();
        if addresses.is_empty() {
            return Err(ListenError::NoAddresses);
        }
        if !addresses
            .iter()
            .all(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast())
        {
            return Err(ListenError::InvalidAddress);
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests;
