//! Sends HTTP requests through local socket files to Caddy's private admin
//! API and the apps tailnet node's `LocalAPI`. The gateway uses these APIs to
//! load routing configuration and read the node's state; it reaches Docker
//! through [`piqueld_docker::Infrastructure`]. Each request has a timeout and
//! an 8 MiB response limit so an unresponsive API cannot leave reconciliation
//! waiting indefinitely.
//!
//! Reads are unrestricted. A mutating request, to any of these APIs or
//! Docker, can only be sent with an open [`Journaled`] action, which commits
//! intent before the request is made. Docker changes go through
//! [`DockerChanges`], whose handle only [`Journaled::change`] lends out.
use crate::operations::OperationError;
use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use piqueld_core::Phase;
use piqueld_core::observability::DiagnosticCode;
use piqueld_docker::InfrastructureChanges;
use piqueld_store::JournalAction;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::net::UnixStream;

/// A non-success API response. The bounded body is kept for daemon logs;
/// public diagnostics record only the status.
#[derive(Debug, thiserror::Error)]
#[error("{path}: HTTP {status}: {body}")]
pub struct ResponseError {
    /// Request path, such as `/config/`.
    path: String,
    /// Status returned by Caddy or the `LocalAPI`.
    pub status: StatusCode,
    /// Lossy UTF-8 response body, truncated to 2048 characters.
    body: String,
}

impl ResponseError {
    /// Passes successful statuses and wraps any other status with its bounded body.
    fn check(path: &str, status: StatusCode, body: &[u8]) -> Result<(), Self> {
        if status.is_success() {
            return Ok(());
        }
        Err(Self {
            path: path.to_owned(),
            status,
            body: String::from_utf8_lossy(body).chars().take(2048).collect(),
        })
    }
}

/// Docker changes to the gateway's containers, networks and images. Its
/// handle is private to this module: only an open [`Journaled`] action
/// reaches it, through [`Journaled::change`].
pub(super) struct DockerChanges(InfrastructureChanges);

impl DockerChanges {
    /// Connects to the engine at `socket`; see [`InfrastructureChanges::connect`].
    pub(super) fn connect(socket: &std::path::Path) -> Result<Self> {
        let changes =
            InfrastructureChanges::connect(socket).context("Docker socket is not UTF-8")?;
        Ok(Self(changes))
    }
}

/// An open journal action for one daemon-owned gateway change.
pub(super) struct Journaled<'a> {
    store: &'a piqueld_store::Store,
    /// The ingress's Docker changes, made only through [`Self::change`].
    docker: &'a InfrastructureChanges,
    action: JournalAction,
    /// Classification of a failed outcome.
    code: DiagnosticCode,
    /// Number of requests committed so far; attempts are numbered from 1.
    requests: AtomicU32,
}

impl<'a> Journaled<'a> {
    /// Commits `action_requested` for the next mutating request. The request
    /// must not be sent if this fails.
    pub(super) async fn request(&self) -> Result<()> {
        let request = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        self.store.action_request(&self.action, request).await?;
        Ok(())
    }

    /// Commits intent for `request`, then sends it. A future sends nothing
    /// until awaited, so a request built first is still never made unless its
    /// intent committed.
    async fn send<T, E>(&self, request: impl Future<Output = Result<T, E>>) -> Result<T>
    where
        anyhow::Error: From<E>,
    {
        self.request().await?;
        Ok(request.await?)
    }

    /// Commits intent for one Docker change, then makes it with the
    /// ingress's change handle:
    ///
    /// ```text
    /// journal.change(|docker| docker.start_container(name)).await?;
    /// ```
    pub(super) async fn change<T, E, F>(
        &self,
        change: impl FnOnce(&'a InfrastructureChanges) -> F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T, E>>,
        anyhow::Error: From<E>,
    {
        self.send(change(self.docker)).await
    }

    /// Records the action's outcome, with a sanitized diagnostic on failure.
    pub(super) async fn finish<T>(self, result: Result<T>) -> Result<T> {
        let diagnostic = result
            .as_ref()
            .err()
            .map(|error| OperationError::ingress_diagnostic(self.code, error));
        let recorded = self.store.finish_action(&self.action, diagnostic).await;
        match (result, recorded) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(error), recorded) => {
                if let Err(journal) = recorded {
                    tracing::error!(error=?journal, "ingress action outcome could not be recorded");
                }
                Err(error)
            }
        }
    }
}

impl super::Ingress {
    /// Opens a daemon-scoped journal action for a gateway pass, run under
    /// `update`, attributed to the deployment the pass serves (see
    /// `requester`). Callers decide from reads first and open an action only
    /// when a change is needed, so unchanged passes record no history.
    pub(super) async fn journal(&self, phase: Phase, resource: &str) -> Result<Journaled<'_>> {
        let requester = self.requester.lock().await.clone();
        self.begin(
            requester.as_deref(),
            DiagnosticCode::IngressUnavailable,
            phase,
            resource,
        )
        .await
    }

    /// Opens an action of the background DNS or certificate loops, whose
    /// failure is classified as `code`. They run outside `update` and serve
    /// no deployment, so the daemon is the actor.
    pub(super) async fn journal_as(
        &self,
        code: DiagnosticCode,
        phase: Phase,
        resource: &str,
    ) -> Result<Journaled<'_>> {
        self.begin(None, code, phase, resource).await
    }

    /// Opens an action requested by `requester`'s operation, if any.
    async fn begin(
        &self,
        requester: Option<&str>,
        code: DiagnosticCode,
        phase: Phase,
        resource: &str,
    ) -> Result<Journaled<'_>> {
        let action = self
            .store
            .begin_daemon_action(requester, phase, Some(resource))
            .await?;
        Ok(Journaled {
            store: &self.store,
            docker: &self.changes.0,
            action,
            code,
            requests: AtomicU32::new(0),
        })
    }
}

#[cfg(test)]
impl super::Ingress {
    /// Docker changes made outside the daemon, which the journal never sees.
    pub(super) fn external(&self) -> &InfrastructureChanges {
        &self.changes.0
    }
}

/// Client for one local HTTP API, identified by its Unix socket path.
/// Opens a fresh connection for each request; constructing it performs no I/O.
pub(super) struct UnixApi {
    socket: PathBuf,
    timeout: Duration,
    /// `Host` header of every request.
    host: &'static str,
}

impl UnixApi {
    /// Uses the caller's timeout for the entire request, including connecting
    /// and reading the response. Ingress supplies the global Docker request
    /// timeout, which its Docker requests also use.
    pub(super) fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self {
            socket,
            timeout,
            host: "localhost",
        }
    }

    /// Sends `host` instead of `localhost`, as Tailscale's `LocalAPI` requires
    /// `local-tailscaled.sock`.
    pub(super) fn with_host(mut self, host: &'static str) -> Self {
        self.host = host;
        self
    }

    /// Sends optional JSON and returns the HTTP status and raw response bytes.
    async fn request(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        tokio::time::timeout(self.timeout, async {
            let stream = UnixStream::connect(&self.socket)
                .await
                .with_context(|| format!("connect to {}", self.socket.display()))?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            let mut drivers = tokio::task::JoinSet::new();
            drivers.spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(?error, "ingress HTTP connection ended");
                }
            });
            let bytes = value
                .map(serde_json::to_vec)
                .transpose()?
                .unwrap_or_default();
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("Host", self.host)
                .header("Content-Type", "application/json")
                .header("Connection", "close")
                .body(Full::new(Bytes::from(bytes)))?;
            let response = sender.send_request(request).await?;
            let status = response.status();
            let bytes = Limited::new(response.into_body(), 8 * 1024 * 1024)
                .collect()
                .await
                .map_err(|error| anyhow::anyhow!("read bounded ingress response: {error}"))?
                .to_bytes();
            Ok((status, bytes.to_vec()))
        })
        .await
        .context("ingress HTTP request timed out")?
    }

    /// Requires a successful HTTP status and decodes JSON, or returns `Null`
    /// for an empty response.
    async fn json(&self, method: Method, path: &str, value: Option<&Value>) -> Result<Value> {
        let (status, body) = self.request(method, path, value).await?;
        ResponseError::check(path, status, &body)?;
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&body).context("decode ingress HTTP response")
        }
    }

    /// Reads and decodes a JSON resource.
    pub(super) async fn get(&self, path: &str) -> Result<Value> {
        self.json(Method::GET, path, None).await
    }

    /// Sends a mutating request after committing its intent to `journal`.
    pub(super) async fn send(
        &self,
        journal: &Journaled<'_>,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Value> {
        journal.send(self.json(method, path, value)).await
    }

    /// Simulates a change made outside the daemon, which the journal never sees.
    #[cfg(test)]
    pub(super) async fn external(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Value> {
        self.json(method, path, value).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn supplied_budget_covers_a_stalled_response_body() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.read_exact(&mut [0; 1]).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let client = UnixApi::new(socket, Duration::from_millis(50));
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.request(Method::GET, "/", None),
        )
        .await
        .expect("the supplied 50ms budget must override the usual request budget")
        .unwrap_err();
        assert!(
            error
                .chain()
                .any(<dyn std::error::Error>::is::<tokio::time::error::Elapsed>)
        );
    }
}
