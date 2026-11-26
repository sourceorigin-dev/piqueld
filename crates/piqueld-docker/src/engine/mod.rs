//! Raw Docker Engine API requests over the engine's Unix socket, for what Bollard
//! cannot express, and the errors they return.
use super::{
    Arc, BollardDocker, BollardResult, Docker, DockerError, ListNodesOptions, Path, ResourceKind,
    ServiceSpec,
};
use futures_util::TryStreamExt as _;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use piqueld_core::wait;
use std::time::Duration;
use tokio::net::UnixStream;

/// Largest service response read: a valid API request may be 2 MiB, and Docker
/// adds service metadata around that specification when it is inspected.
const MAX_SERVICE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Pause before resubmitting a service update Docker answered out of sequence.
const SERVICE_UPDATE_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Complete budget for one raw Engine request, from connect to last byte.
const SERVICE_REQUEST_TIMEOUT: Duration = super::DockerTimeout::Request.duration();

/// Bollard's per-request timeout, in seconds. Bollard only bounds a request up
/// to the response headers, which is why adapter calls additionally run under
/// the module-level deadline wrapper.
const BOLLARD_HEADER_TIMEOUT_SECS: u64 = 120;

#[derive(Debug)]
/// Failures of a raw Engine request, before they are classified publicly.
pub(super) enum WireError {
    /// Transport or local failures that already carry a public classification.
    Public(DockerError),
    /// A non-success HTTP response, kept raw so callers can match exact
    /// Engine errors (not found, update out of sequence) before sanitizing.
    Response {
        /// The request's path, which names resources by ID or name only.
        path: String,
        /// The response's HTTP status.
        status: StatusCode,
        /// The response's raw body.
        body: Vec<u8>,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{path}: Docker returned HTTP {status}: {message}")]
/// Diagnostic source for an Engine error response, with a control-free,
/// length-bounded message. Diagnostics record only its status; logs show
/// the whole response.
pub struct ErrorResponse {
    /// The request's path.
    path: String,
    /// The response's HTTP status.
    status: StatusCode,
    /// Docker's message, without control characters and bounded in length.
    message: String,
}

impl ErrorResponse {
    /// The HTTP status Docker answered with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl From<DockerError> for WireError {
    fn from(error: DockerError) -> Self {
        Self::Public(error)
    }
}

impl WireError {
    /// Converts the failure into a [`DockerError`] labelled with `operation`;
    /// a response becomes an [`ErrorResponse`] source (see [`Self::response`]).
    pub(super) fn sanitized(self, operation: &'static str) -> DockerError {
        match self.response() {
            Ok(response) => DockerError::RequestSource {
                operation,
                source: Box::new(response),
            },
            Err(error) => error,
        }
    }

    /// The Engine's error response, with control characters stripped and at
    /// most 2048 characters of its body kept, or the public failure.
    pub(super) fn response(self) -> Result<ErrorResponse, DockerError> {
        match self {
            Self::Public(error) => Err(error),
            Self::Response { path, status, body } => Ok(ErrorResponse {
                path,
                status,
                message: String::from_utf8_lossy(&body)
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(2_048)
                    .collect(),
            }),
        }
    }

    /// Whether Docker could not publish a host port because another listener
    /// holds it, as its container start response says:
    ///
    /// ```text
    /// ... Bind for 0.0.0.0:80 failed: port is already allocated
    /// ... listen tcp4 0.0.0.0:443: bind: address already in use
    /// ```
    pub(super) fn port_in_use(&self) -> bool {
        let Self::Response { body, .. } = self else {
            return false;
        };
        let body = String::from_utf8_lossy(body);
        ["port is already allocated", "address already in use"]
            .iter()
            .any(|message| body.contains(message))
    }

    /// Returns whether Docker answered with HTTP 404.
    pub(super) fn is_not_found(&self) -> bool {
        self.is_status(StatusCode::NOT_FOUND)
    }

    /// Returns whether Docker answered with `status`.
    pub(super) fn is_status(&self, status: StatusCode) -> bool {
        matches!(self, Self::Response { status: actual, .. } if *actual == status)
    }
}

/// Why a build has no build machine to run on.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReachError {
    /// The build machine is unreachable and the build must not run elsewhere.
    #[error(transparent)]
    Unavailable(#[from] super::BuilderUnavailable),
    /// The build log could not record why the build machine is unreachable.
    #[error(transparent)]
    Log(#[from] crate::command::LogError),
}

impl BollardDocker {
    /// Opens one shared, cheaply cloneable Bollard connection handle. The
    /// socket need not exist yet: the client is created on first use.
    ///
    /// # Errors
    /// Returns a sanitized unavailable error for a non-UTF-8 socket path.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        socket
            .to_str()
            .ok_or(DockerError::Unavailable("connect to Docker Engine"))?;
        Ok(Self {
            docker: Arc::default(),
            socket: Arc::from(socket),
            builder: None,
        })
    }

    /// Builds images on `builder`'s build machine instead of the engine.
    #[must_use]
    pub fn with_builder(mut self, builder: super::RemoteBuilder) -> Self {
        self.builder = Some(Arc::new(builder));
        self
    }

    /// The build machine images build on, if any.
    #[must_use]
    pub fn builder(&self) -> Option<&Arc<super::RemoteBuilder>> {
        self.builder.as_ref()
    }

    /// The registry the build machine pushes to.
    ///
    /// # Errors
    /// [`DockerError::Validation`] when images build on this host, which
    /// pushes none.
    pub(super) fn build_registry(&self) -> Result<&super::BuildRegistry, DockerError> {
        self.builder
            .as_ref()
            .map(|builder| builder.registry())
            .ok_or(DockerError::Validation(
                "reach a build registry without a build machine",
            ))
    }

    /// The typed Bollard client, created on first use. Bollard refuses a
    /// socket that does not exist, so until Docker creates it, every call
    /// fails as unavailable and the next one tries again.
    ///
    /// # Errors
    /// [`DockerError::Unavailable`] while the socket doesn't exist.
    pub fn client(&self) -> Result<&Docker, DockerError> {
        if let Some(docker) = self.docker.get() {
            return Ok(docker);
        }
        let socket = self
            .socket
            .to_str()
            .ok_or(DockerError::Unavailable("connect to Docker Engine"))?;
        let docker = Docker::connect_with_unix(
            socket,
            BOLLARD_HEADER_TIMEOUT_SECS,
            bollard::API_DEFAULT_VERSION,
        )
        .map_err(|error| DockerError::unavailable("connect to Docker Engine", error))?;
        Ok(self.docker.get_or_init(|| docker))
    }

    /// Performs one raw service request through Docker's Unix socket.
    ///
    /// Bollard's service model uses a different health-check key spelling than
    /// the Swarm API. Keeping the request and response bytes here lets the
    /// adapter translate that key and inspect Docker's exact update error:
    /// `spec` (if any) is serialized with `HealthCheck` renamed to `Healthcheck`,
    /// then sent by [`Self::engine_request`].
    async fn service_request(
        &self,
        method: Method,
        path: &str,
        spec: Option<&ServiceSpec>,
    ) -> Result<Vec<u8>, WireError> {
        let body = match spec {
            Some(spec) => serde_json::to_value(spec)
                .and_then(|mut value| {
                    Self::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
                    serde_json::to_vec(&value)
                })
                .map_err(|source| {
                    DockerError::request("serialize service specification", source)
                })?,
            None => Vec::new(),
        };
        self.engine_request(method, path, body).await
    }

    /// Performs one raw Engine request whose response is at most
    /// [`MAX_SERVICE_RESPONSE_BYTES`]; see [`Self::engine_exchange`].
    pub(super) async fn engine_request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, WireError> {
        self.engine_exchange(
            method,
            path,
            body,
            Self::read_response::<MAX_SERVICE_RESPONSE_BYTES>,
        )
        .await
    }

    /// Lists `T`s with a raw `GET` of `path`, decoding the JSON array as it
    /// arrives. Fields `T` leaves out, such as config payloads or service
    /// environments, are skipped rather than held, so a listing needs no
    /// size bound: memory follows the largest `T`, not the whole response.
    pub(super) async fn engine_list<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, WireError> {
        self.engine_exchange(Method::GET, path, Vec::new(), async |response| {
            if !response.status().is_success() {
                return Self::read_response::<MAX_SERVICE_RESPONSE_BYTES>(response)
                    .await
                    .map(|_| Vec::new());
            }
            let data = http_body_util::BodyStream::new(response.into_body())
                .try_filter_map(|frame| std::future::ready(Ok(frame.into_data().ok())))
                .map_err(std::io::Error::other);
            let reader = tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(
                Box::pin(data),
            ));
            let decoded = tokio::task::spawn_blocking(move || {
                serde_json::from_reader(std::io::BufReader::new(reader))
            })
            .await
            .map_err(|source| DockerError::request("decode Docker listing", source))?;
            Ok(decoded.map_err(|source| DockerError::request("decode Docker listing", source))?)
        })
        .await
    }

    /// Performs one raw Engine request with a JSON `body` through Docker's
    /// Unix socket, for endpoints Bollard lacks or misrepresents, and for
    /// the containers and networks piqueld runs itself.
    ///
    /// 1. Connect and perform an HTTP/1 handshake on the Unix socket.
    /// 2. Send one `Connection: close` request and pass the response to
    ///    `read`.
    ///
    /// All steps share one [`SERVICE_REQUEST_TIMEOUT`] deadline; elapsed
    /// deadlines surface as unavailability.
    pub(super) async fn engine_exchange<T>(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        read: impl AsyncFnOnce(hyper::Response<hyper::body::Incoming>) -> Result<T, WireError>,
    ) -> Result<T, WireError> {
        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        let connect = "connect to Docker Engine";
        let stream = tokio::time::timeout_at(deadline, UnixStream::connect(self.socket.as_ref()))
            .await
            .map_err(|source| DockerError::unavailable(connect, source))?
            .map_err(|source| DockerError::unavailable(connect, source))?;
        let open = "open Docker connection";
        let (mut sender, connection) = tokio::time::timeout_at(
            deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|source| DockerError::unavailable(open, source))?
        .map_err(|source| DockerError::request(open, source))?;
        let mut drivers = tokio::task::JoinSet::new();
        drivers.spawn(async move {
            if let Err(source) = connection.await {
                tracing::debug!(error = ?source, "Docker connection driver failed");
            }
        });
        let result = tokio::time::timeout_at(deadline, async {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONNECTION, "close")
                .body(Full::new(Bytes::from(body)))
                .map_err(|source| DockerError::request("build Docker request", source))?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|source| DockerError::request("send Docker request", source))?;
            read(response).await.map_err(|error| match error {
                WireError::Response { status, body, .. } => WireError::Response {
                    path: path.to_owned(),
                    status,
                    body,
                },
                error @ WireError::Public(_) => error,
            })
        })
        .await;
        drivers.abort_all();
        if let Some(Err(source)) = drivers.join_next().await
            && !source.is_cancelled()
        {
            return Err(DockerError::request("join Docker connection", source).into());
        }
        result.map_err(|source| DockerError::unavailable("request Docker", source))?
    }

    /// Collects a bounded response while retaining transport failures.
    ///
    /// Bodies larger than `LIMIT` bytes are rejected with a length limit
    /// cause, and non-success statuses keep their body for exact error
    /// matching; [`Self::engine_exchange`] adds the path.
    pub(super) async fn read_response<const LIMIT: usize>(
        response: hyper::Response<hyper::body::Incoming>,
    ) -> Result<Vec<u8>, WireError> {
        let status = response.status();
        let body = Limited::new(response.into_body(), LIMIT)
            .collect()
            .await
            .map_err(|source| DockerError::RequestSource {
                operation: "read Docker response",
                source,
            })?
            .to_bytes()
            .to_vec();
        if status.is_success() {
            Ok(body)
        } else {
            Err(WireError::Response {
                path: String::new(),
                status,
                body,
            })
        }
    }

    /// Inspects the complete service representation, restoring Bollard's
    /// typed health-check field after Docker's `Healthcheck` response key.
    ///
    /// Returns `None` when the service no longer exists.
    pub(super) async fn inspect_service_wire(
        &self,
        identifier: &str,
    ) -> Result<Option<bollard::models::Service>, DockerError> {
        let bytes = match self
            .service_request(Method::GET, &format!("/services/{identifier}"), None)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.sanitized("inspect service")),
        };
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request("decode service response", source))?;
        Self::rename_swarm_healthcheck(&mut value, "Healthcheck", "HealthCheck");
        serde_json::from_value(value)
            .map(Some)
            .map_err(|source| DockerError::request("decode service response", source))
    }

    /// Creates a service from `spec` through the raw service endpoint.
    pub(super) async fn create_service_wire(&self, spec: &ServiceSpec) -> Result<(), DockerError> {
        self.service_request(Method::POST, "/services/create", Some(spec))
            .await
            .map_err(|error| error.sanitized("create service"))
            .map(|_| ())
    }

    /// Updates a service with a bounded retry for Docker's exact transient
    /// optimistic-concurrency response. Every retry refreshes the current
    /// service version and rechecks ownership before resubmitting the specification.
    /// The immutable service ID prevents a replacement with the same name being updated;
    /// a vanished, replaced, renamed, or foreign service is `OwnershipConflict`.
    pub(super) async fn update_service_wire(
        &self,
        id: &str,
        mut version: u64,
        spec: &ServiceSpec,
    ) -> Result<(), DockerError> {
        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.post_service_update(id, version, spec))
                .await
                .map_err(|source| DockerError::unavailable("update service", source))?
            {
                Ok(()) => return Ok(()),
                Err(error)
                    if Self::update_out_of_sequence(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    wait::pause(SERVICE_UPDATE_RETRY_DELAY).await;
                    let refreshed =
                        tokio::time::timeout_at(deadline, self.inspect_service_wire(id))
                            .await
                            .map_err(|source| {
                                DockerError::unavailable("refresh the service version", source)
                            })??;
                    let refreshed = refreshed.ok_or(DockerError::OwnershipConflict)?;
                    let name = refreshed
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.name.as_deref());
                    if refreshed.id.as_deref() != Some(id) || name != spec.name.as_deref() {
                        return Err(DockerError::OwnershipConflict);
                    }
                    Self::check_ownership(
                        refreshed
                            .spec
                            .as_ref()
                            .and_then(|spec| spec.labels.clone())
                            .unwrap_or_default(),
                        &spec
                            .labels
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                        ResourceKind::Service,
                        spec.name.as_deref().unwrap_or_default(),
                    )?;
                    version = refreshed
                        .version
                        .and_then(|value| value.index)
                        .ok_or(DockerError::Request("read refreshed service version"))?;
                }
                Err(error) => return Err(error.sanitized("update service")),
            }
        }
    }

    /// Submits `spec` once as service `id`'s update at `version`.
    pub(super) async fn post_service_update(
        &self,
        id: &str,
        version: u64,
        spec: &ServiceSpec,
    ) -> Result<(), WireError> {
        self.service_request(
            Method::POST,
            &format!("/services/{id}/update?version={version}&registryAuthFrom=spec"),
            Some(spec),
        )
        .await
        .map(|_| ())
    }

    /// Returns whether Docker reported the one transient update conflict that
    /// is safe for the caller to retry with a refreshed service version.
    fn update_out_of_sequence(error: &WireError) -> bool {
        let WireError::Response { status, body, .. } = error else {
            return false;
        };
        *status == StatusCode::INTERNAL_SERVER_ERROR
            && serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
                .is_some_and(|message| {
                    message == "rpc error: code = Unknown desc = update out of sequence"
                })
    }

    /// Renames the health-check key in either a service spec or a service response.
    ///
    /// Accepts a bare spec or a service object with a `Spec` field; values
    /// without a container spec are left untouched.
    ///
    /// ```text
    /// {"TaskTemplate":{"ContainerSpec":{"HealthCheck":…}}}
    ///   → {"TaskTemplate":{"ContainerSpec":{"Healthcheck":…}}}
    /// ```
    fn rename_swarm_healthcheck(value: &mut serde_json::Value, from: &str, to: &str) {
        let spec = if let Some(spec) = value.get_mut("Spec") {
            spec
        } else {
            value
        };
        let Some(container) = spec
            .get_mut("TaskTemplate")
            .and_then(|task| task.get_mut("ContainerSpec"))
            .and_then(serde_json::Value::as_object_mut)
        else {
            return;
        };
        if let Some(healthcheck) = container.remove(from) {
            container.insert(to.to_owned(), healthcheck);
        }
    }

    /// Fetches the local nodes and rejects anything other than one ready,
    /// reachable, active manager.
    pub(super) async fn validate_single_node_manager(&self) -> Result<(), DockerError> {
        let nodes = self
            .client()?
            .list_nodes(None::<ListNodesOptions>)
            .await
            .or_request("list Swarm nodes")?;
        if !Self::single_node_manager(&nodes) {
            return Err(DockerError::IncompatibleSwarm);
        }
        Ok(())
    }

    /// Reads the daemon's Swarm membership, labelling failures with `operation`.
    pub(super) async fn swarm_info(
        &self,
        operation: &'static str,
    ) -> Result<bollard::models::SwarmInfo, DockerError> {
        let info = self
            .client()?
            .info()
            .await
            .map_err(|source| DockerError::unavailable(operation, source))?;
        Ok(info.swarm.unwrap_or_default())
    }

    /// The build machine a build runs on: the configured one, once it
    /// accepts connections. While it does not, the build fails with
    /// [`super::BuilderUnavailable`], or runs on the engine if
    /// `when_unavailable`, the build's `build.unavailable` setting, is
    /// `local`; `log` says which and why, without the
    /// `BuildKit` daemon's address, which only `system:read` may see.
    pub(super) async fn reachable_builder(
        &self,
        when_unavailable: piqueld_core::build_machine::WhenUnavailable,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> Result<Option<&super::RemoteBuilder>, ReachError> {
        let Some(remote) = self.builder.as_deref() else {
            return Ok(None);
        };
        let Err(unavailable) = remote.probe().await else {
            return Ok(Some(remote));
        };
        let local = when_unavailable == piqueld_core::build_machine::WhenUnavailable::Local;
        tracing::warn!(error = ?unavailable, local, "the build machine is unreachable");
        if let Some(log) = log {
            let outcome = if local {
                "WARN Building on this host instead, as build.unavailable allows"
            } else {
                "ERROR Start the build machine, then deploy again"
            };
            log.append(
                format!(
                    "The build machine's BuildKit daemon is unreachable: {}\n{outcome}.\n",
                    unavailable.source
                )
                .as_bytes(),
                piqueld_core::api::LogStream::Stderr,
            )
            .await?;
        }
        if local {
            Ok(None)
        } else {
            Err(unavailable.into())
        }
    }

    /// The platform the engine runs images of, as Buildx names it, such as
    /// `linux/arm64`: a build machine may have another architecture.
    pub(super) async fn platform(&self) -> Result<String, DockerError> {
        let version = self
            .client()?
            .version()
            .await
            .map_err(|source| DockerError::unavailable("read the engine's platform", source))?;
        match (version.os, version.arch) {
            (Some(os), Some(arch)) => Ok(format!("{os}/{arch}")),
            _ => Err(DockerError::Request("read the engine's platform")),
        }
    }

    /// Returns the local manager's immutable node ID, which pins services to this
    /// daemon. Topology is not checked here, so observation and removals keep
    /// working if another node joins; mutations check it through `ensure_swarm`.
    pub(super) async fn local_node_id(&self) -> Result<String, DockerError> {
        let swarm = self.swarm_info("inspect Docker Swarm state").await?;
        if swarm.control_available != Some(true) {
            return Err(DockerError::NotManager);
        }
        swarm
            .node_id
            .filter(|id| !id.is_empty())
            .ok_or(DockerError::Request("read local Swarm node identity"))
    }

    /// Returns whether Docker reports exactly one ready, reachable manager.
    pub(super) fn single_node_manager(nodes: &[bollard::models::Node]) -> bool {
        nodes.len() == 1
            && nodes[0].spec.as_ref().and_then(|spec| spec.role)
                == Some(bollard::models::NodeSpecRoleEnum::MANAGER)
            && nodes[0].spec.as_ref().and_then(|spec| spec.availability)
                == Some(bollard::models::NodeSpecAvailabilityEnum::ACTIVE)
            && nodes[0].status.as_ref().and_then(|status| status.state)
                == Some(bollard::models::NodeState::READY)
            && nodes[0]
                .manager_status
                .as_ref()
                .and_then(|status| status.reachability)
                == Some(bollard::models::Reachability::REACHABLE)
    }
}

#[cfg(test)]
mod tests;
