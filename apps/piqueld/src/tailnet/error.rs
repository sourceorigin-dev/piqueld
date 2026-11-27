//! Why the tailnet node and its `tailscale` commands fail.

use crate::certificate::LeafError;
use piqueld_config::tailscale::LocalApiError;
use std::{io, net::SocketAddr, path::PathBuf, process::ExitStatus, time::Duration};
use thiserror::Error;
use tokio::time::error::Elapsed;

/// Why running a `tailscale` command failed.
#[derive(Debug, Error)]
pub enum CommandError {
    /// A command other than `up` outlived its timeout.
    #[error("tailscale {operation} timed out after {timeout:?}")]
    TimedOut {
        /// The `tailscale` subcommand.
        operation: String,
        /// The limit it exceeded.
        timeout: Duration,
        /// The elapsed timeout.
        #[source]
        source: Elapsed,
    },
    /// The `tailscale` CLI could not be started.
    #[error("failed to run tailscale")]
    Spawn(#[source] io::Error),
    /// The child has no piped standard output.
    #[error("capture tailscale stdout")]
    CaptureStdout,
    /// The child has no piped standard error.
    #[error("capture tailscale stderr")]
    CaptureStderr,
    /// Standard output or error could not be read.
    #[error("failed to read tailscale {operation} output")]
    ReadOutput {
        /// The `tailscale` subcommand.
        operation: String,
        /// The read failure.
        #[source]
        source: io::Error,
    },
    /// The command's exit could not be awaited.
    #[error("failed to wait for tailscale")]
    Wait(#[source] io::Error),
    /// The command exited unsuccessfully.
    #[error("tailscale {operation} failed ({status}): {stderr}")]
    Failed {
        /// The `tailscale` subcommand.
        operation: String,
        /// How it exited.
        status: ExitStatus,
        /// Its trimmed standard error.
        stderr: String,
    },
}

/// Why the daemon's tailnet node failed to start or keep running.
#[derive(Debug, Error)]
pub enum TailnetError {
    /// The `LocalAPI` could not be read.
    #[error(transparent)]
    LocalApi(#[from] LocalApiError),
    /// `tailscaled` is not logged in and connected.
    #[error("the tailscaled at {} is {state}; log it in first", socket.display())]
    NotRunning {
        /// The `tailscaled`'s socket.
        socket: PathBuf,
        /// Its backend state.
        state: String,
    },
    /// The logged-in node has no `MagicDNS` name.
    #[error("logged-in tailnet node reported no DNS name")]
    NoDnsName,
    /// The `LocalAPI` refused the HTTPS certificate.
    #[error(
        "failed to obtain the tailnet HTTPS certificate; enable HTTPS certificates for the tailnet"
    )]
    Certificate(#[source] LocalApiError),
    /// The certificate's leaf could not be read. Its text already names the
    /// failing part, so it is not repeated as a source.
    #[error("invalid {0}")]
    InvalidCertificate(LeafError),
    /// The loopback listener the HTTPS port forwards to could not be bound.
    #[error("failed to bind the tailnet forwarding listener")]
    BindForwarder(#[source] io::Error),
    /// The loopback listener has no local address.
    #[error(transparent)]
    ForwarderAddress(io::Error),
    /// A shared `tailscaled` already forwards the port to a live listener.
    #[error(
        "tailnet HTTPS port {port} already forwards to {target}; stop that process or set another tailscale.https_port (it still accepts connections)"
    )]
    ForwardAccepting {
        /// The HTTPS port.
        port: u16,
        /// Where it forwards.
        target: SocketAddr,
    },
    /// A shared `tailscaled` forwards the port to an address that failed
    /// without refusing.
    #[error(
        "tailnet HTTPS port {port} already forwards to {target}; stop that process or set another tailscale.https_port"
    )]
    ForwardUnreachable {
        /// The HTTPS port.
        port: u16,
        /// Where it forwards.
        target: SocketAddr,
        /// The connection failure.
        #[source]
        source: io::Error,
    },
    /// A shared `tailscaled` forwards the port to an address that did not
    /// answer in time.
    #[error(
        "tailnet HTTPS port {port} already forwards to {target}; stop that process or set another tailscale.https_port"
    )]
    ForwardTimedOut {
        /// The HTTPS port.
        port: u16,
        /// Where it forwards.
        target: SocketAddr,
        /// The elapsed timeout.
        #[source]
        source: Elapsed,
    },
    /// The port is already in the Serve configuration some other way.
    #[error(
        "tailnet HTTPS port {port} is already in the node's Serve configuration; remove it or set another tailscale.https_port"
    )]
    PortServed {
        /// The HTTPS port.
        port: u16,
    },
    /// Forwarding the HTTPS port failed.
    #[error("failed to forward the tailnet node's HTTPS port")]
    Forward(#[source] CommandError),
    /// Another process took the port while the node started.
    #[error(
        "another process took tailnet HTTPS port {port} over while piqueld started; stop it or set another tailscale.https_port"
    )]
    PortTakenOver {
        /// The HTTPS port.
        port: u16,
    },
    /// The state directory could not be prepared.
    #[error("failed to prepare tailnet state {}", dir.display())]
    PrepareState {
        /// The state directory.
        dir: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// Logging the node in failed.
    #[error("failed to log the tailnet node in")]
    Login(#[source] CommandError),
    /// `tailscaled` exited while the node logged in.
    #[error("tailscaled exited during login: {0}")]
    ExitedDuringLogin(ExitStatus),
    /// The stale `tailscaled` socket could not be removed.
    #[error("failed to remove the stale tailscaled socket")]
    RemoveStaleSocket(#[source] io::Error),
    /// `tailscaled` could not be started.
    #[error("failed to start tailscaled")]
    Start(#[source] io::Error),
    /// The `tailscaled` child has no piped standard error.
    #[error("capture tailscaled logs")]
    CaptureLogs,
    /// `tailscaled` exited before opening its socket.
    #[error("tailscaled exited during startup ({0}); its logs are at debug level")]
    ExitedDuringStartup(ExitStatus),
    /// `tailscaled` did not open its socket in time.
    #[error("tailscaled did not open {} within {timeout:?}", socket.display())]
    StartupTimedOut {
        /// The socket it should have opened.
        socket: PathBuf,
        /// The limit it exceeded.
        timeout: Duration,
    },
    /// A `tailscale` command failed outside the steps that name their own.
    #[error(transparent)]
    Command(#[from] CommandError),
    /// `tailscaled`'s exit status could not be read.
    #[error(transparent)]
    Process(io::Error),
    /// `tailscaled` exited while the node was serving.
    #[error("tailscaled exited unexpectedly ({0})")]
    Exited(ExitStatus),
}
