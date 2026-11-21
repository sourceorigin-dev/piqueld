//! Sends HTTP requests through Caddy's private admin socket with [`UnixHttp`],
//! whose timeout and response limit keep an unresponsive Caddy from leaving
//! reconciliation waiting indefinitely. The gateway loads its routing
//! configuration this way. It reaches Docker through
//! [`piqueld_docker::Infrastructure`], and reads the apps tailnet node through
//! [`piqueld_config::tailscale::LocalApi`].
//!
//! Reads are unrestricted. A mutating request, to Caddy or Docker, can only
//! be sent with an open [`Journaled`] action, which commits intent before the
//! request is made. Docker changes go through [`DockerChanges`], whose handle
//! only [`Journaled::change`] lends out.
use crate::operations::OperationError;
use anyhow::{Context, Result};
use hyper::Method;
pub use piqueld_config::unix_http::ResponseError;
use piqueld_config::unix_http::UnixHttp;
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

/// Client for Caddy's admin API, on its Unix socket, in JSON.
pub(super) struct CaddyApi {
    http: UnixHttp,
}

impl CaddyApi {
    /// Uses the caller's timeout for the entire request, including connecting
    /// and reading the response. Ingress supplies the global Docker request
    /// timeout, which its Docker requests also use.
    pub(super) fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self {
            http: UnixHttp::new(socket, "localhost", timeout),
        }
    }

    /// Requires a successful HTTP status and decodes JSON, or returns `Null`
    /// for an empty response.
    async fn json(&self, method: Method, path: &str, value: Option<&Value>) -> Result<Value> {
        let body = self.http.checked(method, path, value).await?;
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
