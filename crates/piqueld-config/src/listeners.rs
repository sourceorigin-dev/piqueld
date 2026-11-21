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
mod tests {
    use super::*;
    use crate::tailscale::FakeLocalApi;

    #[test]
    fn modes_select_only_the_requested_interfaces() {
        let tailscale = vec![
            "100.100.1.2".parse().unwrap(),
            "fd7a:115c:a1e0::2".parse().unwrap(),
        ];
        for (mode, expected) in [
            (ListenMode::Off, vec![]),
            (ListenMode::Localhost, vec!["127.0.0.1:7845", "[::1]:7845"]),
            (
                ListenMode::Tailscale,
                vec!["100.100.1.2:7845", "[fd7a:115c:a1e0::2]:7845"],
            ),
            (
                ListenMode::Both,
                vec![
                    "127.0.0.1:7845",
                    "[::1]:7845",
                    "100.100.1.2:7845",
                    "[fd7a:115c:a1e0::2]:7845",
                ],
            ),
        ] {
            let config = ServerConfig {
                listen_mode: mode,
                ..ServerConfig::default()
            };
            assert_eq!(
                config.listen_addresses(tailscale.clone()),
                expected
                    .iter()
                    .map(|s| s.parse::<SocketAddr>().unwrap())
                    .collect::<Vec<_>>()
            );
            let unavailable = config.listen_addresses(Vec::new());
            assert!(unavailable.iter().all(|address| address.ip().is_loopback()));
        }
    }

    /// The host's status, as `tailscaled` 1.104 reports it with these
    /// addresses, trimmed.
    fn status(state: &str, addresses: &str) -> String {
        format!(
            r#"{{"BackendState":"{state}","TailscaleIPs":{addresses},
            "Self":{{"DNSName":"host.tail1234.ts.net.","TailscaleIPs":{addresses}}}}}"#
        )
    }

    #[test]
    fn discovery_requires_running_state_and_valid_addresses() {
        let parse = |json: &str| {
            serde_json::from_str::<Status>(json)
                .map_err(anyhow::Error::from)
                .and_then(|status| status.listen_addresses())
        };
        let valid = status(
            "Running",
            r#"["100.100.1.2","fd7a:115c:a1e0::2","100.100.1.2"]"#,
        );
        assert_eq!(parse(&valid).unwrap().len(), 2);
        for output in [
            status("Stopped", r#"["100.100.1.2"]"#),
            r#"{"BackendState":"NeedsLogin","Self":null}"#.to_owned(),
            status("NeedsLogin", "null"),
            status("Running", "[]"),
            status("Running", r#"["0.0.0.0"]"#),
            status("Running", r#"["::"]"#),
            status("Running", r#"["bad"]"#),
            "not JSON".to_owned(),
        ] {
            assert!(parse(&output).is_err(), "accepted {output}");
        }
    }

    /// Discovery reads `tailscaled`'s status, and reports a stopped
    /// `tailscaled` or a missing socket.
    #[tokio::test]
    async fn discovery_reads_the_local_api_and_reports_failures() {
        let running = status("Running", r#"["100.100.1.2","fd7a:115c:a1e0::2"]"#);
        let fake =
            FakeLocalApi::serve(&[("/localapi/v0/status?peers=false", 200, &running)]).unwrap();
        assert_eq!(
            Status::discover(&fake.api()).await.unwrap(),
            vec![
                "100.100.1.2".parse::<IpAddr>().unwrap(),
                "fd7a:115c:a1e0::2".parse().unwrap()
            ]
        );
        let stopped = status("Stopped", "[]");
        let fake =
            FakeLocalApi::serve(&[("/localapi/v0/status?peers=false", 200, &stopped)]).unwrap();
        let error = Status::discover(&fake.api()).await.unwrap_err();
        assert_eq!(error.to_string(), "Tailscale is Stopped");
        let api = LocalApi::new("/nonexistent/tailscaled.sock".into(), DISCOVERY_TIMEOUT);
        let error = format!("{:#}", Status::discover(&api).await.unwrap_err());
        assert!(error.contains("/nonexistent/tailscaled.sock"), "{error}");
    }

    /// A `tailscaled` that accepts but never answers is given up on.
    #[tokio::test(start_paused = true)]
    async fn discovery_timeout_stops_waiting_for_tailscaled() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("tailscaled.sock");
        let _stuck = tokio::net::UnixListener::bind(&socket).unwrap();
        let api = LocalApi::new(socket, DISCOVERY_TIMEOUT);
        let error = format!("{:#}", Status::discover(&api).await.unwrap_err());
        assert!(error.contains("timed out after 5s"), "{error}");
    }

    #[tokio::test]
    async fn bind_conflict_is_fatal_and_releases_previous_listeners() {
        let occupied = TcpListener::bind("[::1]:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let config = ServerConfig {
            listen_mode: ListenMode::Localhost,
            port,
            ..ServerConfig::default()
        };
        let error = config.bind_tcp().await.unwrap_err();
        assert!(error.to_string().contains(&format!("[::1]:{port}")));
        TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }
}
