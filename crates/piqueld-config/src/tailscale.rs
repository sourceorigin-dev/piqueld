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
mod tests {
    use super::*;

    /// The node's status as `tailscaled` 1.104 reports it, trimmed.
    const RUNNING: &str = r#"{"Version":"1.104.6","BackendState":"Running","AuthURL":"",
        "TailscaleIPs":["100.123.77.68","fd7a:115c:a1e0::c538:4d44"],
        "Self":{"HostName":"piqueld","DNSName":"piqueld.tail1234.ts.net.",
        "TailscaleIPs":["100.123.77.68","fd7a:115c:a1e0::c538:4d44"]}}"#;

    #[tokio::test]
    async fn status_reads_the_node_without_its_peers() {
        let fake =
            FakeLocalApi::serve(&[("/localapi/v0/status?peers=false", 200, RUNNING)]).unwrap();
        let status = fake.api().status().await.unwrap();
        assert!(status.running());
        assert_eq!(status.dns_name().unwrap(), "piqueld.tail1234.ts.net");
        assert_eq!(status.addresses().count(), 2);

        let fake = FakeLocalApi::serve(&[(
            "/localapi/v0/status?peers=false",
            200,
            r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/1","Self":{"DNSName":"","TailscaleIPs":null}}"#,
        )]).unwrap();
        let status = fake.api().status().await.unwrap();
        assert!(!status.running());
        assert_eq!(status.auth_url, "https://login.tailscale.com/a/1");
        assert_eq!(status.dns_name(), None);
        assert_eq!(status.addresses().count(), 0);
    }

    #[tokio::test]
    async fn whois_reads_users_and_treats_tagged_nodes_as_userless() {
        let fake = FakeLocalApi::serve(&[
            (
                "/localapi/v0/whois?addr=100.64.0.1",
                200,
                r#"{"Node":{"Name":"laptop.tail1234.ts.net.","Tags":null},
                "UserProfile":{"LoginName":"alice@example.com"}}"#,
            ),
            (
                "/localapi/v0/whois?addr=fd7a:115c:a1e0::2",
                200,
                r#"{"Node":{"Name":"runner.tail1234.ts.net.","Tags":["tag:ci"]},
                "UserProfile":{"LoginName":"tagged-devices"}}"#,
            ),
            (
                "/localapi/v0/whois?addr=100.64.0.9",
                404,
                "no match for IP:port\n",
            ),
        ])
        .unwrap();
        let api = fake.api();
        let laptop = api.whois("100.64.0.1".parse().unwrap()).await.unwrap();
        assert_eq!(laptop.login.as_deref(), Some("alice@example.com"));
        assert_eq!(laptop.node, "laptop");
        let runner = api
            .whois("fd7a:115c:a1e0::2".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(runner.login, None);
        assert_eq!(runner.tags, ["tag:ci"]);
        let unknown = api.whois("100.64.0.9".parse().unwrap()).await.unwrap_err();
        assert!(format!("{unknown:#}").contains("HTTP 404"), "{unknown:#}");
        assert!(format!("{unknown:#}").contains("no match"), "{unknown:#}");
    }

    #[tokio::test]
    async fn certificates_are_read_as_pem_and_refusals_are_errors() {
        let fake = FakeLocalApi::serve(&[
            (
                "/localapi/v0/cert/piqueld.tail1234.ts.net?type=cert",
                200,
                "-----BEGIN CERTIFICATE-----\n",
            ),
            (
                "/localapi/v0/cert/other.tail1234.ts.net?type=cert",
                500,
                "invalid domain \"other.tail1234.ts.net\"\n",
            ),
        ])
        .unwrap();
        let api = fake.api();
        let pem = api.certificate("piqueld.tail1234.ts.net").await.unwrap();
        assert_eq!(pem, b"-----BEGIN CERTIFICATE-----\n");
        let refused = api.certificate("other.tail1234.ts.net").await.unwrap_err();
        assert!(
            format!("{refused:#}").contains("invalid domain"),
            "{refused:#}"
        );
    }

    #[tokio::test]
    async fn serve_config_reads_forwards_and_nothing_served() {
        let fake = FakeLocalApi::serve(&[(
            "/localapi/v0/serve-config",
            200,
            r#"{"TCP":{"7001":{"TCPForward":"127.0.0.1:4000","TerminateTLS":"node.ts.net","ProxyProtocol":2},
            "443":{"HTTPS":true}},"Web":{}}"#,
        )]).unwrap();
        let config = fake.api().serve_config().await.unwrap();
        assert_eq!(
            config.port(7001),
            PortUse::Forwarded("127.0.0.1:4000".parse().unwrap())
        );
        assert_eq!(config.port(443), PortUse::Other);
        assert_eq!(config.port(7002), PortUse::Unserved);
        for empty in ["null", "{}"] {
            let fake = FakeLocalApi::serve(&[("/localapi/v0/serve-config", 200, empty)]).unwrap();
            let config = fake.api().serve_config().await.unwrap();
            assert_eq!(config.port(443), PortUse::Unserved, "{empty}");
        }
    }

    /// Without a `tailscaled`, or with one that stopped and left its socket,
    /// every read fails naming the socket.
    #[tokio::test]
    async fn reads_fail_when_tailscaled_is_down() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.sock");
        let stale = directory.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        for socket in [missing, stale] {
            let api = LocalApi::new(socket.clone(), Duration::from_secs(5));
            let error = format!("{:#}", api.status().await.unwrap_err());
            assert!(error.contains(&socket.display().to_string()), "{error}");
            assert!(error.contains("connect"), "{error}");
        }
    }
}
