//! The contract and implementation of `piqueld-probe`, the in-container HTTP
//! health probe.
//!
//! The daemon embeds the static probe, mounts it at [`CONTAINER_PATH`] as a
//! Swarm config into services whose health check runs it, and points Docker's
//! health check at it, so Swarm keeps gating tasks and rollouts on health
//! while images need no `wget`, shell, or C library:
//!
//! ```text
//! /run/piqueld/probe http --port 8080 --path /health/ready --timeout 3
//! ```
//!
//! The probe makes one plain HTTP GET to the IPv4 loopback address and
//! succeeds on a 2xx or 3xx status. The daemon's drift checks parse the vectors
//! of deployed services, which older daemons authored, so the argument format
//! is a stable contract: only extend it compatibly.

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    num::{NonZeroU16, NonZeroU32},
    time::{Duration, Instant},
};

/// Where piqueld mounts the probe inside service containers.
pub const CONTAINER_PATH: &str = "/run/piqueld/probe";

/// Longest response head read before the response is rejected as malformed.
const MAX_HEAD: usize = 8192;

/// One HTTP health check against a container-local port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpProbe {
    /// Loopback port to connect to.
    pub port: u16,
    /// Request path, starting with `/` and free of whitespace and controls.
    pub path: String,
    /// Bound on connecting, sending, and reading the status line, in seconds.
    pub timeout_seconds: u32,
}

/// Arguments that do not describe an [`HttpProbe`].
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArgumentError {
    /// The arguments do not start with the `http` subcommand.
    #[error("usage: {CONTAINER_PATH} http --port PORT --path PATH --timeout SECONDS")]
    Usage,
    /// A flag is unknown, repeated, or lacks its value.
    #[error("unexpected argument {0:?}")]
    Unexpected(String),
    /// A required flag is absent.
    #[error("missing {0}")]
    Missing(&'static str),
    /// A flag's value is out of range or malformed.
    #[error("invalid {flag} value {value:?}")]
    Invalid {
        /// The flag whose value was rejected.
        flag: &'static str,
        /// The rejected value.
        value: String,
    },
}

/// Why a probe found the service unhealthy.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The connection could not be established.
    #[error("connect to {address}")]
    Connect {
        /// The loopback address probed.
        address: SocketAddr,
        /// The socket failure, such as a refused connection or timeout.
        #[source]
        source: io::Error,
    },
    /// The request could not be sent or the status line could not be read.
    #[error("exchange the request")]
    Exchange(#[source] io::Error),
    /// The timeout elapsed before a status line arrived.
    #[error("no response within {0}s")]
    Timeout(u32),
    /// The server did not answer with an HTTP status line.
    #[error("malformed status line {0:?}")]
    Malformed(String),
    /// The server answered with a status other than 2xx or 3xx.
    #[error("unhealthy status {0}")]
    Status(u16),
}

impl HttpProbe {
    /// Returns the command Docker runs for this probe.
    ///
    /// ```text
    /// ["/run/piqueld/probe","http","--port","8080","--path","/health","--timeout","3"]
    /// ```
    #[must_use]
    pub fn command(&self) -> Vec<String> {
        [
            CONTAINER_PATH,
            "http",
            "--port",
            &self.port.to_string(),
            "--path",
            &self.path,
            "--timeout",
            &self.timeout_seconds.to_string(),
        ]
        .map(String::from)
        .into()
    }

    /// Parses the arguments after the executable, accepting flags in any order.
    ///
    /// # Errors
    /// Returns an [`ArgumentError`] for a missing subcommand, unknown,
    /// repeated, or missing flags, a zero port or timeout, or a path that does
    /// not start with `/` or contains whitespace or control characters.
    #[expect(
        clippy::map_err_ignore,
        reason = "the error names the flag and the rejected value, which a parse error cannot improve on"
    )]
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, ArgumentError> {
        let mut arguments = arguments.into_iter();
        if arguments.next().as_deref() != Some("http") {
            return Err(ArgumentError::Usage);
        }
        let (mut port, mut path, mut timeout) = (None, None, None);
        while let Some(flag) = arguments.next() {
            let slot = match flag.as_str() {
                "--port" => &mut port,
                "--path" => &mut path,
                "--timeout" => &mut timeout,
                _ => return Err(ArgumentError::Unexpected(flag)),
            };
            if slot.is_some() {
                return Err(ArgumentError::Unexpected(flag));
            }
            *slot = Some(arguments.next().ok_or(ArgumentError::Unexpected(flag))?);
        }
        let invalid = |flag, value: String| ArgumentError::Invalid { flag, value };
        let port = port.ok_or(ArgumentError::Missing("--port"))?;
        let path = path.ok_or(ArgumentError::Missing("--path"))?;
        let timeout = timeout.ok_or(ArgumentError::Missing("--timeout"))?;
        Ok(Self {
            port: port
                .parse::<NonZeroU16>()
                .map_err(|_| invalid("--port", port))?
                .get(),
            timeout_seconds: timeout
                .parse::<NonZeroU32>()
                .map_err(|_| invalid("--timeout", timeout))?
                .get(),
            path: if path.starts_with('/')
                && !path.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                path
            } else {
                return Err(invalid("--path", path));
            },
        })
    }

    /// Sends one `GET` to `127.0.0.1:port` and returns its final 2xx or 3xx
    /// status, after any interim 1xx responses. Redirects are not followed and
    /// the body is not read.
    ///
    /// # Errors
    /// Returns a [`ProbeError`] when connecting or exchanging fails, the
    /// timeout elapses, or the status is malformed or outside 200..=399.
    pub fn check(&self) -> Result<u16, ProbeError> {
        let deadline = Instant::now() + Duration::from_secs(self.timeout_seconds.into());
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ProbeError::Timeout(self.timeout_seconds))
        };
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&address, remaining()?)
            .map_err(|source| ProbeError::Connect { address, source })?;
        stream
            .set_write_timeout(Some(remaining()?))
            .and_then(|()| {
                // One write, so the request leaves in as few segments as possible.
                let request = format!(
                    "GET {} HTTP/1.1\r\nHost: {address}\r\nUser-Agent: piqueld-probe\r\nConnection: close\r\n\r\n",
                    self.path
                );
                stream.write_all(request.as_bytes())
            })
            .map_err(ProbeError::Exchange)?;

        // Interim 1xx responses precede the final one; skip their heads. Only
        // a complete head counts, so a truncated response is never healthy.
        let mut response = Vec::new();
        loop {
            let end = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|end| end + 4);
            if end.unwrap_or(response.len()) > MAX_HEAD {
                return Err(ProbeError::Malformed(
                    String::from_utf8_lossy(&response[..MAX_HEAD]).into_owned(),
                ));
            }
            if let Some(end) = end {
                let head = response.drain(..end).collect::<Vec<_>>();
                match Self::status(&head)? {
                    100..200 => continue,
                    status @ 200..400 => return Ok(status),
                    status => return Err(ProbeError::Status(status)),
                }
            }
            let mut buffer = [0; 256];
            stream
                .set_read_timeout(Some(remaining()?))
                .map_err(ProbeError::Exchange)?;
            match stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(ProbeError::Malformed(
                        String::from_utf8_lossy(&response).into_owned(),
                    ));
                }
                Ok(read) => response.extend_from_slice(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(ProbeError::Timeout(self.timeout_seconds));
                }
                Err(error) => return Err(ProbeError::Exchange(error)),
            }
        }
    }

    /// Returns the three-digit status code of a response head's status line.
    fn status(head: &[u8]) -> Result<u16, ProbeError> {
        let line = head
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .map_or(head, |line| &head[..line]);
        let line = String::from_utf8_lossy(line).into_owned();
        line.strip_prefix("HTTP/")
            .and_then(|rest| rest.split(' ').nth(1))
            .filter(|code| code.len() == 3)
            .and_then(|code| code.parse().ok())
            .ok_or(ProbeError::Malformed(line))
    }
}

#[cfg(test)]
mod tests;
