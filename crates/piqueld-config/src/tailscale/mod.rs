//! Reads a `tailscaled`'s state through its `LocalAPI`, the HTTP API its
//! socket serves and the `tailscale` CLI uses. The daemon's tailnet node, the
//! apps node and the Tailscale listen mode read their `tailscaled` this way;
//! the daemon still runs the CLI to log its node in and change Serve.
//!
//! ```text
//! status        GET /localapi/v0/status?peers=false
//! whois         GET /localapi/v0/whois?addr=<ip>
//! certificate   GET /localapi/v0/cert/<name>?type=cert
//! serve config  GET /localapi/v0/serve-config
//! ```

use crate::unix_http::UnixHttp;
use anyhow::{Context, Result};
use hyper::Method;
use piqueld_core::tailnet::TailnetPeer;
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

/// The socket of the host's `tailscaled` on Linux, which the `tailscale` CLI
/// uses unless told otherwise.
pub const HOST_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Client for one `tailscaled`'s `LocalAPI`. Constructing it performs no I/O.
pub struct LocalApi {
    http: UnixHttp,
}

impl LocalApi {
    /// The `LocalAPI` on `socket`, bounding each request by `timeout`.
    #[must_use]
    pub fn new(socket: PathBuf, timeout: Duration) -> Self {
        // tailscaled refuses requests for any other host.
        Self {
            http: UnixHttp::new(socket, "local-tailscaled.sock", timeout),
        }
    }

    /// The `tailscaled`'s socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        self.http.socket()
    }

    /// Reads `/localapi/v0/<path>`, requiring a successful status.
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.http
            .checked(Method::GET, &format!("/localapi/v0/{path}"), None)
            .await
            .with_context(|| format!("failed to ask tailscaled at {}", self.socket().display()))
    }

    /// Reads and decodes `/localapi/v0/<path>`; `what` names it in errors.
    async fn get<T: DeserializeOwned>(&self, path: &str, what: &str) -> Result<T> {
        serde_json::from_slice(&self.read(path).await?)
            .with_context(|| format!("invalid tailscale {what}"))
    }

    /// The node's own status, without its peers.
    ///
    /// # Errors
    /// Fails when `tailscaled` doesn't answer or answers with an error.
    pub async fn status(&self) -> Result<Status> {
        self.get("status?peers=false", "status").await
    }

    /// Who the tailnet node at `address` is.
    ///
    /// # Errors
    /// Fails when `tailscaled` doesn't answer, or knows no node there.
    pub async fn whois(&self, address: IpAddr) -> Result<TailnetPeer> {
        let response: WhoIs = self.get(&format!("whois?addr={address}"), "whois").await?;
        Ok(response.into_peer())
    }

    /// The PEM certificate chain for the node's DNS name `name`. Like
    /// `tailscale cert`, `tailscaled` issues it on first use and renews it
    /// before it expires.
    ///
    /// # Errors
    /// Fails when `tailscaled` doesn't answer or can't issue the certificate.
    pub async fn certificate(&self, name: &str) -> Result<Vec<u8>> {
        self.read(&format!("cert/{name}?type=cert")).await
    }

    /// The node's Serve configuration.
    ///
    /// # Errors
    /// Fails when `tailscaled` doesn't answer or answers with an error.
    pub async fn serve_config(&self) -> Result<ServeConfig> {
        // `null` means nothing is served.
        let config: Option<ServeConfig> = self.get("serve-config", "serve config").await?;
        Ok(config.unwrap_or_default())
    }
}

/// The fields piqueld reads from a `tailscaled`'s status.
#[derive(Debug, Deserialize)]
pub struct Status {
    /// The node's state, such as `Running` or `NeedsLogin`.
    #[serde(rename = "BackendState")]
    pub backend_state: String,
    /// Interactive login URL while the node needs login.
    #[serde(rename = "AuthURL", default)]
    pub auth_url: String,
    #[serde(rename = "Self")]
    node: Option<NodeStatus>,
}

#[derive(Debug, Deserialize)]
struct NodeStatus {
    #[serde(rename = "DNSName")]
    dns_name: String,
    /// `null` until the node has logged in.
    #[serde(rename = "TailscaleIPs")]
    addresses: Option<Vec<IpAddr>>,
}

impl Status {
    /// Whether the node is logged in and connected.
    #[must_use]
    pub fn running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// The node's DNS name, without its trailing dot, once it has one.
    #[must_use]
    pub fn dns_name(&self) -> Option<String> {
        self.node
            .as_ref()
            .map(|node| node.dns_name.trim_end_matches('.').to_owned())
            .filter(|name| !name.is_empty())
    }

    /// The node's tailnet addresses, none before it has logged in.
    pub fn addresses(&self) -> impl Iterator<Item = IpAddr> {
        self.node
            .iter()
            .flat_map(|node| node.addresses.iter().flatten().copied())
    }
}

/// The fields piqueld reads from a `whois` answer.
#[derive(Deserialize)]
struct WhoIs {
    #[serde(rename = "Node")]
    node: WhoIsNode,
    #[serde(rename = "UserProfile")]
    user: Option<WhoIsUser>,
}

#[derive(Deserialize)]
struct WhoIsNode {
    /// Fully qualified name, e.g. `laptop.tail1234.ts.net.`.
    #[serde(rename = "Name")]
    name: String,
    /// Absent or null for untagged nodes.
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct WhoIsUser {
    #[serde(rename = "LoginName")]
    login: String,
}

impl WhoIs {
    /// Tagged nodes belong to no user, whatever profile `tailscaled` reports.
    fn into_peer(self) -> TailnetPeer {
        let node = self
            .node
            .name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_owned();
        let tags = self.node.tags.unwrap_or_default();
        let login = self.user.filter(|_| tags.is_empty()).map(|user| user.login);
        TailnetPeer { login, node, tags }
    }
}

/// The node's TCP ports in its Serve configuration, by number.
#[derive(Default, Deserialize)]
pub struct ServeConfig {
    #[serde(rename = "TCP", default)]
    tcp: BTreeMap<u16, PortHandler>,
}

#[derive(Deserialize)]
struct PortHandler {
    /// `<host>:<port>` a TCP forward sends connections to.
    #[serde(rename = "TCPForward")]
    tcp_forward: Option<String>,
}

/// How the node serves one TCP port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortUse {
    /// Not in the Serve configuration.
    Unserved,
    /// Forwarded to this address, such as another piqueld's listener.
    Forwarded(SocketAddr),
    /// Served some other way, such as a web handler, which piqueld can't
    /// tell is in use.
    Other,
}

impl ServeConfig {
    /// How `port` is served.
    #[must_use]
    pub fn port(&self, port: u16) -> PortUse {
        match self.tcp.get(&port) {
            None => PortUse::Unserved,
            Some(handler) => handler
                .tcp_forward
                .as_deref()
                .and_then(|target| target.parse().ok())
                .map_or(PortUse::Other, PortUse::Forwarded),
        }
    }
}

/// A stand-in `tailscaled` serving recorded `LocalAPI` answers on a socket in
/// a temporary directory, for tests. It stops when dropped.
#[cfg(any(test, feature = "testing"))]
pub struct FakeLocalApi {
    socket: PathBuf,
    _directory: tempfile::TempDir,
    _server: tokio::task::JoinSet<()>,
}

#[cfg(any(test, feature = "testing"))]
impl FakeLocalApi {
    /// Answers each `(path, status, body)`, such as
    /// `("/localapi/v0/status?peers=false", 200, "{…}")`, matching the path
    /// with its query exactly; any other path gets a 404.
    ///
    /// Must run inside a Tokio runtime.
    ///
    /// # Errors
    /// Fails when the socket can't be bound.
    pub fn serve(answers: &[(&str, u16, &str)]) -> std::io::Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("tailscaled.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let answers: Vec<(String, u16, String)> = answers
            .iter()
            .map(|&(path, status, body)| (path.to_owned(), status, body.to_owned()))
            .collect();
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                // Requests are bodiless GETs: read up to the end of the head.
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).await.is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let path = head.split(' ').nth(1).unwrap_or_default();
                let (status, body) = answers
                    .iter()
                    .find(|(answered, ..)| answered == path)
                    .map_or((404, "no such path"), |(_, status, body)| (*status, body));
                let response = format!(
                    "HTTP/1.1 {status} Fake\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "the fake LocalAPI peer may have hung up"
                )]
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Ok(Self {
            socket,
            _directory: directory,
            _server: server,
        })
    }

    /// The fake's socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A client of the fake.
    #[must_use]
    pub fn api(&self) -> LocalApi {
        LocalApi::new(self.socket.clone(), Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests;
