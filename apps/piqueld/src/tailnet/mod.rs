//! The daemon's own tailnet node. It serves the website over HTTPS with the
//! tailnet-issued certificate, independently of the host's Tailscale daemon.
//!
//! piqueld supervises a dedicated `tailscaled` in userspace-networking mode, or
//! shares a configured one. It reads the node's state through the `LocalAPI`
//! (see [`piqueld_config::tailscale`]), and runs the `tailscale` CLI to log the
//! node in and change its Serve configuration, which the `LocalAPI` can't do
//! as simply. So the CLI (and `tailscaled`, unless shared) must be on `PATH`.

mod error;
mod node;
mod proxy;
mod whois;

pub use error::{CommandError, TailnetError};
pub use node::{Node, NodeListener};
pub use proxy::ProxyListener;
pub use whois::{LookupTimedOut, TailnetLookup, Whois};

use piqueld_config::tailscale::{LocalApi, LocalApiError, PortUse};
use std::{io::ErrorKind, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::TcpStream,
    process::Command,
};

/// Bounds `LocalAPI` reads and non-interactive commands, allowing certificate
/// issuance time.
const COMMAND_TIMEOUT: Duration = Duration::from_mins(2);
/// A live loopback listener accepts at once; this only bounds a stuck one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The node's `tailscaled`: read through its `LocalAPI`, and changed with the
/// `tailscale` CLI pointed at the same socket.
struct Tailscaled {
    /// Client of the node's `tailscaled` `LocalAPI`.
    api: LocalApi,
}

impl Tailscaled {
    /// The `tailscaled` listening on `socket`.
    fn at(socket: PathBuf) -> Self {
        Self {
            api: LocalApi::new(socket, COMMAND_TIMEOUT),
        }
    }

    /// Fails unless piqueld may take `port`, served as `serving`, over: it
    /// is unserved, or forwards to an address that refuses connections, such
    /// as that of a daemon that crashed without removing it. Only a refusal
    /// shows the owner is gone: a timeout may be a live owner that is stuck.
    async fn check_takeover(port: u16, serving: PortUse) -> Result<(), TailnetError> {
        match serving {
            PortUse::Unserved => Ok(()),
            PortUse::Forwarded(target) => {
                match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
                    Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
                        tracing::info!(port, %target, "replacing the tailnet HTTPS port's stale forward");
                        Ok(())
                    }
                    Ok(Ok(_)) => Err(TailnetError::ForwardAccepting { port, target }),
                    Ok(Err(source)) => Err(TailnetError::ForwardUnreachable {
                        port,
                        target,
                        source,
                    }),
                    Err(source) => Err(TailnetError::ForwardTimedOut {
                        port,
                        target,
                        source,
                    }),
                }
            }
            PortUse::Other => Err(TailnetError::PortServed { port }),
        }
    }

    /// How the node serves `port`.
    async fn serving(&self, port: u16) -> Result<PortUse, LocalApiError> {
        Ok(self.api.serve_config().await?.port(port))
    }

    /// Runs `tailscale <args>` to completion and returns its stdout. Stderr is
    /// logged as it arrives, which is how interactive login URLs from `up`
    /// reach the operator, and is included in the error on failure. Only `up`
    /// may wait indefinitely for interactive approval.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, CommandError> {
        let operation = args.first().copied().unwrap_or_default();
        Self::run_command(
            Command::new("tailscale")
                .arg(format!("--socket={}", self.api.socket().display()))
                .args(args),
            operation,
        )
        .await
    }

    /// Runs `command` to completion and returns its standard output; fails with `operation` in the
    /// context if it exits unsuccessfully.
    async fn run_command(command: &mut Command, operation: &str) -> Result<Vec<u8>, CommandError> {
        let output = Self::output(command, operation);
        if operation == "up" {
            output.await
        } else {
            tokio::time::timeout(COMMAND_TIMEOUT, output)
                .await
                .map_err(|source| CommandError::TimedOut {
                    operation: operation.to_owned(),
                    timeout: COMMAND_TIMEOUT,
                    source,
                })?
        }
    }

    /// Dropping this future on timeout or shutdown kills the CLI child.
    async fn output(command: &mut Command, operation: &str) -> Result<Vec<u8>, CommandError> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(CommandError::Spawn)?;
        let mut stdout = child.stdout.take().ok_or(CommandError::CaptureStdout)?;
        let stderr = child.stderr.take().ok_or(CommandError::CaptureStderr)?;
        let (stdout, stderr) = tokio::try_join!(
            async {
                let mut output = Vec::new();
                stdout.read_to_end(&mut output).await.map(|_| output)
            },
            async {
                let (mut lines, mut output) = (BufReader::new(stderr).lines(), String::new());
                while let Some(line) = lines.next_line().await? {
                    let line = line.trim();
                    if !line.is_empty() {
                        tracing::info!("tailscale {operation}: {line}");
                        output.push_str(line);
                        output.push('\n');
                    }
                }
                Ok(output)
            },
        )
        .map_err(|source| CommandError::ReadOutput {
            operation: operation.to_owned(),
            source,
        })?;
        let status = child.wait().await.map_err(CommandError::Wait)?;
        if !status.success() {
            return Err(CommandError::Failed {
                operation: operation.to_owned(),
                status,
                stderr: stderr.trim().to_owned(),
            });
        }
        Ok(stdout)
    }

    /// Fetches the certificate for the node's name and returns its expiry.
    /// tailscaled caches it in the node state and renews it before expiry.
    async fn certificate_expiry_ms(&self, dns_name: &str) -> Result<i64, TailnetError> {
        let pem = self
            .api
            .certificate(dns_name)
            .await
            .map_err(TailnetError::Certificate)?;
        let validity = crate::certificate::Validity::of_leaf(&pem)
            .map_err(TailnetError::InvalidCertificate)?;
        Ok(validity.not_after_ms)
    }
}

#[cfg(test)]
mod tests;
