//! The containers, bridge networks and images piqueld runs itself, outside
//! Swarm: ingress's Caddy gateway, apps tailnet node and `cloudflared`.
//!
//! Container specs and inspections stay Docker's JSON, which ingress builds
//! and compares field by field. Requests go raw to the engine socket through
//! [`BollardDocker::engine_exchange`], each under the request budget, except
//! pulls, which go through Bollard. Reads ([`Infrastructure`]) and changes
//! ([`InfrastructureChanges`]) are separate handles: nothing here is
//! journaled, so callers keep the changes behind their journal.
use super::{BollardDocker, DockerError, ImageSource, engine::WireError};
use hyper::Method;
use serde_json::{Value, json};
use std::path::Path;

/// The Docker Engine API version requests use: the oldest that supports the
/// `GwPriority` of [`EngineVersion::supports_gateway_priority`]. Pinned so a
/// newer engine keeps answering in the shapes callers read.
const API_VERSION: &str = "1.48";
/// Largest response read, such as 100 lines of a container's logs.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Characters kept of each log line.
const MAX_LOG_LINE_CHARS: usize = 4096;

/// Docker Engine and API versions, e.g. `28.1.1` and `1.49`.
#[derive(Debug)]
pub struct EngineVersion {
    /// Docker Engine's version; empty when the engine doesn't report one.
    engine: String,
    /// The newest Engine API version it serves; empty when unreported.
    api: String,
}

impl EngineVersion {
    /// Whether the engine is Docker Engine 28+ with API 1.48+, which support
    /// the per-endpoint `GwPriority` [`InfrastructureChanges::connect_network`]
    /// sets.
    ///
    /// ```text
    /// {"Version": "28.1.1", "ApiVersion": "1.49"}  -> true
    /// {"Version": "27.5.0", "ApiVersion": "1.47"}  -> false
    /// ```
    #[must_use]
    pub fn supports_gateway_priority(&self) -> bool {
        let major = self
            .engine
            .split('.')
            .next()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let api = self.api.split_once('.').and_then(|(major, minor)| {
            Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
        });
        major >= 28 && api.is_some_and(|v| v >= (1, 48))
    }
}

impl std::fmt::Display for EngineVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Docker Engine {} with API {}", self.engine, self.api)
    }
}

/// Reads of the engine, for the containers, networks and images piqueld runs
/// itself. Connecting performs no I/O.
pub struct Infrastructure {
    docker: BollardDocker,
}

/// Changes to the containers, networks and images piqueld runs itself. A
/// handle of its own, apart from [`Infrastructure`]'s reads, so a caller can
/// keep it behind its journal. Connecting performs no I/O.
pub struct InfrastructureChanges(Infrastructure);

impl Infrastructure {
    /// Connects to the engine at `socket`, which need not exist yet.
    ///
    /// # Errors
    /// [`DockerError::Unavailable`] for a non-UTF-8 socket path.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        Ok(Self {
            docker: BollardDocker::connect(socket)?,
        })
    }

    /// `path` under the pinned [`API_VERSION`], except `/version`, so an
    /// engine too old for it still reports its version instead of rejecting
    /// the request.
    ///
    /// ```text
    /// /containers/gateway/json -> /v1.48/containers/gateway/json
    /// /version                 -> /version
    /// ```
    fn versioned(path: &str) -> String {
        if path == "/version" {
            path.to_owned()
        } else {
            format!("/v{API_VERSION}{path}")
        }
    }

    /// Sends one request with an optional JSON `body` to the [versioned
    /// path](Self::versioned) and returns the bytes of a successful response.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Vec<u8>, WireError> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|source| DockerError::request("encode Docker request", source))?
            .unwrap_or_default();
        self.docker
            .engine_exchange(
                method,
                &Self::versioned(path),
                body,
                BollardDocker::read_response::<MAX_RESPONSE_BYTES>,
            )
            .await
    }

    /// Sends one request and decodes its JSON response (see [`Self::decode`]),
    /// labelling a failure with `operation`.
    async fn send(
        &self,
        operation: &'static str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, DockerError> {
        let bytes = self
            .request(method, path, body)
            .await
            .map_err(|error| error.sanitized(operation))?;
        Self::decode(operation, &bytes)
    }

    /// Reads the JSON at `path`: `None` when Docker answers 404. Other
    /// failures stay errors, so an outage is never mistaken for absence.
    async fn inspect(
        &self,
        operation: &'static str,
        path: &str,
    ) -> Result<Option<Value>, DockerError> {
        match self.request(Method::GET, path, None).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| DockerError::request(operation, source)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.sanitized(operation)),
        }
    }

    /// Decodes a JSON response; an empty one, as a container start answers,
    /// is `Null`.
    fn decode(operation: &'static str, bytes: &[u8]) -> Result<Value, DockerError> {
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(bytes).map_err(|source| DockerError::request(operation, source))
    }

    /// The engine's versions, as `GET /version` reports them.
    ///
    /// # Errors
    /// The failed request.
    pub async fn version(&self) -> Result<EngineVersion, DockerError> {
        let version = self
            .send("read Docker version", Method::GET, "/version", None)
            .await?;
        let field = |name: &str| version[name].as_str().unwrap_or_default().to_owned();
        Ok(EngineVersion {
            engine: field("Version"),
            api: field("ApiVersion"),
        })
    }

    /// Inspects the container `name`, if it exists.
    ///
    /// # Errors
    /// The failed request.
    pub async fn inspect_container(&self, name: &str) -> Result<Option<Value>, DockerError> {
        self.inspect("inspect container", &format!("/containers/{name}/json"))
            .await
    }

    /// The IDs of the running containers that carry every one of `labels`,
    /// each a `key=value`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn running_containers(
        &self,
        labels: Vec<String>,
    ) -> Result<Vec<String>, DockerError> {
        let operation = "list containers";
        let filters = serde_json::to_string(&BollardDocker::label_filter(labels))
            .map_err(|source| DockerError::request(operation, source))?;
        let path = format!(
            "/containers/json?filters={}",
            url::form_urlencoded::byte_serialize(filters.as_bytes()).collect::<String>()
        );
        let containers = self.send(operation, Method::GET, &path, None).await?;
        Ok(containers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|container| container["Id"].as_str().map(str::to_owned))
            .collect())
    }

    /// Inspects the network `name`, if it exists.
    ///
    /// # Errors
    /// The failed request.
    pub async fn inspect_network(&self, name: &str) -> Result<Option<Value>, DockerError> {
        self.inspect("inspect network", &format!("/networks/{name}"))
            .await
    }

    /// Whether the engine has `image`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn has_image(&self, image: &str) -> Result<bool, DockerError> {
        Ok(self
            .inspect("inspect image", &format!("/images/{image}/json"))
            .await?
            .is_some())
    }

    /// Reads the last `tail` lines the container `name` wrote to stdout and
    /// stderr, or only those between `between`'s Unix seconds, each
    /// truncated to [`MAX_LOG_LINE_CHARS`]. Docker multiplexes both streams
    /// into frames:
    ///
    /// ```text
    /// [stream: u8][0; 3][length: u32 big-endian][payload: length bytes] ...
    /// ```
    ///
    /// # Errors
    /// The failed request, or a truncated frame.
    pub async fn log_lines(
        &self,
        name: &str,
        tail: u16,
        between: Option<(u64, u64)>,
    ) -> Result<Vec<String>, DockerError> {
        let window = between.map_or_else(String::new, |(since, until)| {
            format!("&since={since}&until={until}")
        });
        let path = format!("/containers/{name}/logs?stdout=1&stderr=1&tail={tail}{window}");
        let bytes = self
            .request(Method::GET, &path, None)
            .await
            .map_err(|error| error.sanitized("read container logs"))?;
        let mut lines = Vec::new();
        let mut remaining = bytes.as_slice();
        while let Some((header, rest)) = remaining.split_first_chunk::<8>() {
            let [.., a, b, c, d] = *header;
            let length = u32::from_be_bytes([a, b, c, d]) as usize;
            let (payload, rest) = rest
                .split_at_checked(length)
                .ok_or(DockerError::Request("decode truncated log frame"))?;
            for line in String::from_utf8_lossy(payload).lines() {
                lines.push(line.chars().take(MAX_LOG_LINE_CHARS).collect());
            }
            remaining = rest;
        }
        Ok(lines)
    }
}

impl InfrastructureChanges {
    /// Connects to the engine at `socket`, which need not exist yet.
    ///
    /// # Errors
    /// [`DockerError::Unavailable`] for a non-UTF-8 socket path.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        Infrastructure::connect(socket).map(Self)
    }

    /// Sends one change, ignoring its response, labelling a failure with
    /// `operation`.
    async fn change(
        &self,
        operation: &'static str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(), DockerError> {
        self.0.send(operation, method, path, body).await.map(drop)
    }

    /// Pulls `image` like image resolution does, failing on an error in its
    /// progress stream. Callers bound how long it takes.
    ///
    /// # Errors
    /// The failed pull.
    pub async fn pull_image(&self, image: &str) -> Result<(), DockerError> {
        ImageSource::pull(self.0.docker.client()?, image).await
    }

    /// Creates the container `name` from `spec` without starting it.
    ///
    /// # Errors
    /// The failed request.
    pub async fn create_container(&self, name: &str, spec: &Value) -> Result<(), DockerError> {
        let path = format!("/containers/create?name={name}");
        self.change("create container", Method::POST, &path, Some(spec))
            .await
    }

    /// Starts the container `name`.
    ///
    /// # Errors
    /// [`DockerError::PortInUse`] when another listener holds a host port it
    /// publishes, or the failed request.
    pub async fn start_container(&self, name: &str) -> Result<(), DockerError> {
        let operation = "start container";
        let path = format!("/containers/{name}/start");
        match self.0.request(Method::POST, &path, None).await {
            Ok(bytes) => Infrastructure::decode(operation, &bytes).map(drop),
            Err(error) if error.port_in_use() => Err(error
                .response()
                .map_or_else(|error| error, DockerError::PortInUse)),
            Err(error) => Err(error.sanitized(operation)),
        }
    }

    /// Stops the container `name`, killing it after `seconds`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn stop_container(&self, name: &str, seconds: u32) -> Result<(), DockerError> {
        let path = format!("/containers/{name}/stop?t={seconds}");
        self.change("stop container", Method::POST, &path, None)
            .await
    }

    /// Waits until the container `name` is not running and returns its exit
    /// code, if Docker reports one.
    ///
    /// # Errors
    /// The failed request.
    pub async fn wait_container(&self, name: &str) -> Result<Option<i64>, DockerError> {
        let path = format!("/containers/{name}/wait?condition=not-running");
        let exit = self
            .0
            .send("wait for container", Method::POST, &path, None)
            .await?;
        Ok(exit["StatusCode"].as_i64())
    }

    /// Renames the container `from` to `to`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn rename_container(&self, from: &str, to: &str) -> Result<(), DockerError> {
        let path = format!("/containers/{from}/rename?name={to}");
        self.change("rename container", Method::POST, &path, None)
            .await
    }

    /// Force-removes the container `id`, running or not.
    ///
    /// # Errors
    /// The failed request, including a missing container.
    pub async fn remove_container(&self, id: &str) -> Result<(), DockerError> {
        let path = format!("/containers/{id}?force=true");
        self.change("remove container", Method::DELETE, &path, None)
            .await
    }

    /// Creates a bridge network `name` labelled with `labels`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn create_bridge_network(
        &self,
        name: &str,
        labels: &Value,
    ) -> Result<(), DockerError> {
        let network = json!({"Name":name,"Driver":"bridge","Labels":labels});
        self.change(
            "create network",
            Method::POST,
            "/networks/create",
            Some(&network),
        )
        .await
    }

    /// Removes the network `id`.
    ///
    /// # Errors
    /// The failed request.
    pub async fn remove_network(&self, id: &str) -> Result<(), DockerError> {
        let path = format!("/networks/{id}");
        self.change("remove network", Method::DELETE, &path, None)
            .await
    }

    /// Connects `container` to `network` with gateway priority 0, so the
    /// network it was created on stays its default route.
    ///
    /// # Errors
    /// The failed request.
    pub async fn connect_network(&self, network: &str, container: &str) -> Result<(), DockerError> {
        let path = format!("/networks/{network}/connect");
        let endpoint = json!({"Container":container,"EndpointConfig":{"GwPriority":0}});
        self.change("connect network", Method::POST, &path, Some(&endpoint))
            .await
    }

    /// Disconnects `container` from `network`, without force.
    ///
    /// # Errors
    /// The failed request.
    pub async fn disconnect_network(
        &self,
        network: &str,
        container: &str,
    ) -> Result<(), DockerError> {
        let path = format!("/networks/{network}/disconnect");
        let endpoint = json!({"Container":container,"Force":false});
        self.change("disconnect network", Method::POST, &path, Some(&endpoint))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_priority_needs_engine_28_and_api_1_48() {
        let version = |engine: &str, api: &str| EngineVersion {
            engine: engine.into(),
            api: api.into(),
        };
        assert!(version("28.1.1", "1.49").supports_gateway_priority());
        assert!(version("29.0.0", "1.48").supports_gateway_priority());
        assert!(!version("27.5.0", "1.47").supports_gateway_priority());
        assert!(!version("28.0.0", "1.47").supports_gateway_priority());
        assert!(!version("", "").supports_gateway_priority());
    }

    /// Every request but `/version` goes to the pinned API version, on the
    /// wire as the engine sees it.
    #[tokio::test]
    async fn requests_pin_the_api_version_except_version() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let directory = tempfile::tempdir().expect("the temp dir is created");
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("the listener binds");
        let server = tokio::spawn(async move {
            let mut lines = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("the connection is accepted");
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(stream.read_u8().await.expect("the request is read"));
                }
                let head = String::from_utf8(head).expect("the bytes are utf-8");
                lines.push(
                    head.lines()
                        .next()
                        .expect("the iterator yields an item")
                        .to_owned(),
                );
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .expect("the response is written");
            }
            lines
        });
        let docker = Infrastructure::connect(&socket).expect("the docker client connects");
        docker.version().await.expect("the version is read");
        docker
            .inspect_container("gateway")
            .await
            .expect("the container is inspected");
        docker
            .log_lines("gateway", 20, Some((1, 2)))
            .await
            .expect("the log lines are read");
        let changes = InfrastructureChanges::connect(&socket).expect("the docker client connects");
        changes
            .remove_network("edge")
            .await
            .expect("the network is removed");
        assert_eq!(
            server.await.expect("the server task finishes"),
            [
                "GET /version HTTP/1.1",
                "GET /v1.48/containers/gateway/json HTTP/1.1",
                "GET /v1.48/containers/gateway/logs?stdout=1&stderr=1&tail=20&since=1&until=2 HTTP/1.1",
                "DELETE /v1.48/networks/edge HTTP/1.1",
            ]
        );
    }
}
