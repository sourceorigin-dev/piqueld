//! Startup-only TCP listener selection and Tailscale discovery.

use super::{ListenMode, ServerConfig};
use crate::tailscale::{HOST_SOCKET, LocalApi, Status};
use anyhow::{Context, Result, ensure};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::net::TcpListener;

/// Bounds discovery, so a stuck `tailscaled` can't hold startup up.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

impl ServerConfig {
    /// Binds every configured TCP address before the daemon starts serving.
    /// Unavailable Tailscale is warning-only and is never retried.
    ///
    /// # Errors
    /// Any bind failure aborts startup and releases listeners already bound.
    pub async fn bind_tcp(&self) -> Result<Vec<TcpListener>> {
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
                    .with_context(|| format!("failed to bind HTTP API on {address}"))?,
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
    async fn discover(api: &LocalApi) -> Result<Vec<IpAddr>> {
        api.status()
            .await
            .context("failed to read the host's Tailscale status")?
            .listen_addresses()
    }

    /// Extracts sorted, deduplicated addresses, requiring a `Running` backend and a
    /// non-empty list in which every address is concrete (not unspecified,
    /// loopback, or multicast).
    fn listen_addresses(&self) -> Result<Vec<IpAddr>> {
        ensure!(self.running(), "Tailscale is {}", self.backend_state);
        let mut addresses: Vec<IpAddr> = self.addresses().collect();
        ensure!(!addresses.is_empty(), "Tailscale has no assigned addresses");
        // Never turn malformed discovery output into a wildcard listener.
        ensure!(
            addresses
                .iter()
                .all(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()),
            "Tailscale reported an invalid interface address"
        );
        addresses.sort_unstable();
        addresses.dedup();
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests;
