//! HTTP/1.1 through a local socket file, to Caddy's private admin API and
//! tailscaled's `LocalAPI`. Each request has a timeout and an
//! 8 MiB response limit, so an unresponsive API cannot leave its caller
//! waiting indefinitely.
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::UnixStream;

/// The largest response body read.
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

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
    ///
    /// # Errors
    /// Returns the error for any status but a success.
    pub fn check(path: &str, status: StatusCode, body: &[u8]) -> Result<(), Self> {
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

/// Why a request to a local HTTP API failed.
#[derive(Debug, thiserror::Error)]
pub enum UnixHttpError {
    /// The socket refused the connection.
    #[error("connect to {}", socket.display())]
    Connect {
        /// The API's socket.
        socket: PathBuf,
        /// The connection failure.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP exchange failed.
    #[error(transparent)]
    Exchange(#[from] hyper::Error),
    /// The request body is not serializable.
    #[error(transparent)]
    Encode(#[from] serde_json::Error),
    /// The request could not be built.
    #[error(transparent)]
    Request(#[from] hyper::http::Error),
    /// The response body failed or exceeded its limit.
    #[error("read bounded {path} response: {cause}")]
    Body {
        /// Request path.
        path: String,
        /// The read failure.
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The request took longer than its timeout.
    #[error("{path} through {} timed out after {timeout:?}", socket.display())]
    Timeout {
        /// Request path.
        path: String,
        /// The API's socket.
        socket: PathBuf,
        /// The timeout that elapsed.
        timeout: Duration,
        /// The elapsed timer.
        #[source]
        source: tokio::time::error::Elapsed,
    },
    /// The API answered with an error status.
    #[error(transparent)]
    Response(#[from] ResponseError),
}

impl UnixHttpError {
    /// The error status the API answered with, if that is what failed.
    #[must_use]
    pub fn response(&self) -> Option<&ResponseError> {
        match self {
            Self::Response(error) => Some(error),
            _ => None,
        }
    }
}

/// Client for one local HTTP API, identified by its Unix socket path.
/// Opens a fresh connection for each request; constructing it performs no I/O.
pub struct UnixHttp {
    socket: PathBuf,
    timeout: Duration,
    /// `Host` header of every request.
    host: &'static str,
}

impl UnixHttp {
    /// Uses `timeout` for each entire request, including connecting and
    /// reading the response, and sends `host` as its `Host` header.
    #[must_use]
    pub fn new(socket: PathBuf, host: &'static str, timeout: Duration) -> Self {
        Self {
            socket,
            timeout,
            host,
        }
    }

    /// The API's socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sends optional JSON and returns the HTTP status and raw response bytes.
    ///
    /// # Errors
    /// Fails when the socket refuses the connection, the exchange fails, the
    /// response exceeds 8 MiB, or the request takes longer than its timeout.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>), UnixHttpError> {
        tokio::time::timeout(self.timeout, async {
            let stream = UnixStream::connect(&self.socket).await.map_err(|source| {
                UnixHttpError::Connect {
                    socket: self.socket.clone(),
                    source,
                }
            })?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            let mut drivers = tokio::task::JoinSet::new();
            drivers.spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(?error, "local HTTP connection ended");
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
            let bytes = Limited::new(response.into_body(), RESPONSE_LIMIT)
                .collect()
                .await
                .map_err(|cause| UnixHttpError::Body {
                    path: path.to_owned(),
                    cause,
                })?
                .to_bytes();
            Ok((status, bytes.to_vec()))
        })
        .await
        .map_err(|source| UnixHttpError::Timeout {
            path: path.to_owned(),
            socket: self.socket.clone(),
            timeout: self.timeout,
            source,
        })?
    }

    /// Requires a successful HTTP status and returns the response's bytes.
    ///
    /// # Errors
    /// Fails as [`Self::request`] does, or with a [`ResponseError`] for any
    /// other status.
    pub async fn checked(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Vec<u8>, UnixHttpError> {
        let (status, body) = self.request(method, path, value).await?;
        ResponseError::check(path, status, &body)?;
        Ok(body)
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
        let client = UnixHttp::new(socket, "localhost", Duration::from_millis(50));
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.request(Method::GET, "/", None),
        )
        .await
        .expect("the supplied 50ms budget must override the usual request budget")
        .unwrap_err();
        assert!(matches!(error, UnixHttpError::Timeout { .. }));
    }
}
