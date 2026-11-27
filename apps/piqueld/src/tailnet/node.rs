//! A `tailscaled` owns the node. It terminates HTTPS on the configured port
//! with the tailnet certificate and forwards plain TCP, prefixed with a PROXY
//! v2 header, to a loopback listener that the website is served on.
//!
//! piqueld normally starts a dedicated `tailscaled`. With `tailscale.socket`,
//! it shares one that is already logged in, which several daemons can serve
//! from, each on its own port.

use super::TailnetError;
use super::{ProxyListener, Tailscaled};
use axum::serve::{ListenerExt, TapIo};
use piqueld_config::DaemonConfig;
use piqueld_config::tailscale::PortUse;
use piqueld_core::ErrorChain as _;
use piqueld_core::api::TailnetStatus;
use piqueld_core::wait;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// tailscaled renews certificates itself; refreshing keeps the reported
/// certificate expiry and login state current.
const REFRESH_INTERVAL: Duration = Duration::from_mins(1);
/// tailscaled normally opens its socket well within a second.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause between checks for tailscaled's socket during startup.
const STARTUP_POLL: Duration = Duration::from_millis(100);
/// How long after tailscaled exits a shutdown still explains the exit.
const SHUTDOWN_RACE: Duration = Duration::from_secs(1);

/// A logged-in tailnet node forwarding its HTTPS port to piqueld.
pub struct Node {
    /// The `tailscaled` behind the node.
    daemon: Daemon,
    /// Supervises `tailscaled` and publishes the node's status.
    monitor: Monitor,
    /// Loopback listener the HTTPS port forwards to.
    listener: TcpListener,
}

/// The `tailscaled` behind the node.
enum Daemon {
    /// Started by piqueld, which stops it on exit and exits if it does.
    Owned(Child),
    /// Shared with other processes. piqueld only removes its HTTPS port from
    /// the Serve configuration on exit, if it still forwards to piqueld.
    Shared,
}

/// The node's HTTPS listener. The no-op `tap_io` wrapper is what lets axum
/// supply `ConnectInfo<SocketAddr>`, which authentication throttling needs;
/// axum otherwise derives it only for TCP listeners.
pub type NodeListener = TapIo<ProxyListener, fn(&mut TcpStream)>;

impl Node {
    /// Starts the node when `tailscale.enabled` is set and waits until it is
    /// logged in, holds a certificate, and forwards `tailscale.https_port` to
    /// piqueld. An unset `auth.public_url` becomes the node's HTTPS origin.
    ///
    /// Without an auth key or saved state, a dedicated node needs an
    /// interactive login; the login URL is logged and startup waits for it. A
    /// shared node must already be logged in.
    ///
    /// # Errors
    /// Returns state directory, tailscaled, login, certificate, or forwarding
    /// errors. A shared node fails when the port is already served, unless it
    /// forwards to an address that refuses connections.
    pub async fn join(config: &mut DaemonConfig) -> Result<Option<Self>, TailnetError> {
        if !config.tailscale.enabled {
            return Ok(None);
        }
        let (daemon, tailscaled) = match &config.tailscale.socket {
            Some(socket) => (Daemon::Shared, Tailscaled::at(socket.clone())),
            None => Self::start(config).await?,
        };

        let status = tailscaled.api.status().await?;
        if !status.running() {
            return Err(TailnetError::NotRunning {
                socket: tailscaled.api.socket().to_owned(),
                state: status.backend_state,
            });
        }
        let dns_name = status.dns_name().ok_or(TailnetError::NoDnsName)?;
        let expires_at_ms = tailscaled.certificate_expiry_ms(&dns_name).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(TailnetError::BindForwarder)?;
        let address = listener
            .local_addr()
            .map_err(TailnetError::ForwarderAddress)?;
        let https_port = config.tailscale.https_port.get();
        if matches!(daemon, Daemon::Shared) {
            Tailscaled::check_takeover(https_port, tailscaled.serving(https_port).await?).await?;
        }
        tailscaled
            .run(&[
                "serve",
                "--bg",
                &format!("--tls-terminated-tcp={https_port}"),
                "--proxy-protocol=2",
                &format!("tcp://{address}"),
            ])
            .await
            .map_err(TailnetError::Forward)?;
        if tailscaled.serving(https_port).await? != PortUse::Forwarded(address) {
            return Err(TailnetError::PortTakenOver { port: https_port });
        }

        let url = match https_port {
            443 => format!("https://{dns_name}"),
            port => format!("https://{dns_name}:{port}"),
        };
        let public_url_matches = match &config.auth.public_url {
            None => {
                config.auth.public_url = Some(url.clone());
                true
            }
            Some(public_url) => piqueld_config::AuthConfig::validate_origin(public_url)
                .is_ok_and(|origin| origin.origin().ascii_serialization() == url),
        };
        if !public_url_matches {
            tracing::warn!(public_url = config.public_url(), node = %url,
                "auth.public_url is not the tailnet node's origin; passkeys only work on auth.public_url");
        }
        tracing::info!(%url, "tailnet node is serving HTTPS");
        let observation = Observation {
            state: status.backend_state,
            certificate_error: false,
            expires_at_ms,
        };
        Ok(Some(Self {
            daemon,
            monitor: Monitor {
                status: watch::Sender::new(observation.report(
                    &dns_name,
                    &url,
                    public_url_matches,
                    piqueld_store::now_ms(),
                )),
                tailscaled,
                dns_name,
                url,
                https_port,
                public_url_matches,
                expires_at_ms,
                address,
            },
            listener,
        }))
    }

    /// Starts a dedicated tailscaled with state in the data directory and
    /// logs it in.
    async fn start(config: &DaemonConfig) -> Result<(Daemon, Tailscaled), TailnetError> {
        let dir = config.server.tailscale_dir();
        piqueld_config::directories::prepare_data_dir(&dir)
            .await
            .map_err(|source| TailnetError::PrepareState {
                dir: dir.clone(),
                source,
            })?;
        let tailscaled = Tailscaled::at(dir.join("tailscaled.sock"));
        let mut daemon = Self::spawn(&dir, tailscaled.api.socket()).await?;

        let hostname = format!("--hostname={}", config.tailscale.hostname);
        let auth_key = config
            .tailscale
            .auth_key_file
            .as_ref()
            .map(|file| format!("--auth-key=file:{}", file.path().display()));
        let mut up = vec!["up", "--reset", &hostname];
        up.extend(auth_key.as_deref());
        tokio::select! {
            exit = daemon.wait() => {
                return Err(TailnetError::ExitedDuringLogin(exit.map_err(TailnetError::Process)?));
            }
            result = tailscaled.run(&up) => result.map_err(TailnetError::Login)?,
        };
        Ok((Daemon::Owned(daemon), tailscaled))
    }

    /// Starts tailscaled with private state and waits for its socket. Its logs
    /// are forwarded at debug level. It shares piqueld's process group, so
    /// terminal interrupts stop both, and is killed when piqueld drops it.
    async fn spawn(dir: &Path, socket: &Path) -> Result<Child, TailnetError> {
        piqueld_config::directories::remove_file(socket)
            .await
            .map_err(TailnetError::RemoveStaleSocket)?;
        let mut daemon = Command::new("tailscaled")
            .arg("--tun=userspace-networking")
            .arg("--port=0")
            .arg("--no-logs-no-support")
            .arg(format!("--statedir={}", dir.display()))
            .arg(format!("--socket={}", socket.display()))
            .env("TS_LOGS_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(TailnetError::Start)?;
        let mut logs =
            BufReader::new(daemon.stderr.take().ok_or(TailnetError::CaptureLogs)?).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = logs.next_line().await {
                tracing::debug!("tailscaled: {line}");
            }
        });
        let started = Instant::now();
        while !socket.exists() {
            if let Some(exit) = daemon.try_wait().map_err(TailnetError::Process)? {
                return Err(TailnetError::ExitedDuringStartup(exit));
            }
            if started.elapsed() >= STARTUP_TIMEOUT {
                return Err(TailnetError::StartupTimedOut {
                    socket: socket.to_owned(),
                    timeout: STARTUP_TIMEOUT,
                });
            }
            wait::pause(STARTUP_POLL).await;
        }
        Ok(daemon)
    }

    /// Subscribes to the node's periodically refreshed status.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<TailnetStatus> {
        self.monitor.status.subscribe()
    }

    /// Identifies peers connecting through this node.
    #[must_use]
    pub fn whois(&self) -> std::sync::Arc<super::Whois> {
        std::sync::Arc::new(super::Whois::at(
            self.monitor.tailscaled.api.socket().to_owned(),
        ))
    }

    /// The node's `MagicDNS` name, without the trailing dot.
    #[must_use]
    pub fn dns_name(&self) -> &str {
        &self.monitor.dns_name
    }

    /// Leaves the node when startup fails before [`Node::listener`]: a shared
    /// tailscaled stops forwarding the HTTPS port, and a dedicated one is
    /// killed on drop.
    pub async fn leave(self) {
        if let Daemon::Shared = self.daemon {
            self.monitor.remove_port().await;
        }
    }

    /// Starts accepting forwarded connections and refreshing status until
    /// cancellation. Returns the listener and the supervisor task, which
    /// cancels the daemon and fails if its own tailscaled exits, so piqueld
    /// exits with an error and the service manager restarts both.
    #[must_use]
    pub fn listener(
        self,
        cancellation: CancellationToken,
    ) -> (NodeListener, JoinHandle<Result<(), TailnetError>>) {
        let listener =
            ProxyListener::spawn(self.listener, self.monitor.address, cancellation.clone());
        let supervisor = tokio::spawn(self.monitor.run(self.daemon, cancellation));
        let tap: fn(&mut TcpStream) = |_| {};
        (listener.tap_io(tap), supervisor)
    }
}

/// Supervises tailscaled and keeps the reported status current.
struct Monitor {
    /// The node's `tailscaled`.
    tailscaled: Tailscaled,
    /// The node's name on the tailnet.
    dns_name: String,
    /// The node's HTTPS URL.
    url: String,
    /// Port the node serves HTTPS on.
    https_port: u16,
    /// Whether `auth.public_url` is the node's URL.
    public_url_matches: bool,
    /// When the node's certificate expires, in Unix milliseconds.
    expires_at_ms: i64,
    /// The loopback listener the HTTPS port forwards to.
    address: SocketAddr,
    /// Where the node's status is published.
    status: watch::Sender<TailnetStatus>,
}

impl Monitor {
    /// Owns a dedicated tailscaled until cancellation, then drops (and so
    /// kills) it, failing if it exits on its own. A shared tailscaled keeps
    /// running, so its HTTPS port is removed instead.
    async fn run(
        mut self,
        daemon: Daemon,
        cancellation: CancellationToken,
    ) -> Result<(), TailnetError> {
        match daemon {
            Daemon::Owned(daemon) => {
                Self::supervise(daemon, cancellation, self.refresh_loop()).await
            }
            Daemon::Shared => {
                tokio::select! {
                    () = cancellation.cancelled() => {}
                    () = self.refresh_loop() => {}
                }
                self.remove_port().await;
                Ok(())
            }
        }
    }

    /// Stops forwarding this daemon's HTTPS port from a shared tailscaled,
    /// which would otherwise keep forwarding it to a closed listener. A port
    /// another daemon has taken over since is left alone.
    async fn remove_port(&self) {
        let result: Result<(), TailnetError> = async {
            let serving = self.tailscaled.serving(self.https_port).await?;
            if serving != PortUse::Forwarded(self.address) {
                tracing::info!(
                    port = self.https_port,
                    "the tailnet HTTPS port no longer forwards to this daemon; leaving it"
                );
                return Ok(());
            }
            let port = format!("--tls-terminated-tcp={}", self.https_port);
            self.tailscaled
                .run(&["serve", &port, "off"])
                .await
                .map(drop)
                .map_err(TailnetError::from)
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(error = %error.chain(), "failed to remove the tailnet HTTPS port");
        }
    }

    /// Keep supervision active even while a refresh waits for tailscaled.
    async fn supervise(
        mut daemon: Child,
        cancellation: CancellationToken,
        refresh: impl Future<Output = ()>,
    ) -> Result<(), TailnetError> {
        tokio::select! {
            () = cancellation.cancelled() => Ok(()),
            exit = daemon.wait() => {
                if tokio::time::timeout(SHUTDOWN_RACE, cancellation.cancelled()).await.is_ok() {
                    return Ok(());
                }
                tracing::error!(?exit, "tailscaled exited; stopping piqueld");
                cancellation.cancel();
                Err(TailnetError::Exited(exit.map_err(TailnetError::Process)?))
            }
            () = refresh => Ok(()),
        }
    }

    /// Refreshes the status every refresh interval, forever.
    async fn refresh_loop(&mut self) {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REFRESH_INTERVAL,
            REFRESH_INTERVAL,
        );
        loop {
            ticker.tick().await;
            self.refresh().await;
        }
    }

    /// Reads the node's login state and certificate expiry and publishes the status.
    async fn refresh(&mut self) {
        let state = match self.tailscaled.api.status().await {
            Ok(status) => status.backend_state,
            Err(error) => {
                tracing::warn!(error = %error.chain(), "tailnet status refresh failed");
                "Unknown".into()
            }
        };
        let certificate_error = match self.tailscaled.certificate_expiry_ms(&self.dns_name).await {
            Ok(expires_at_ms) => {
                self.expires_at_ms = expires_at_ms;
                false
            }
            Err(error) => {
                tracing::warn!(error = %error.chain(), "tailnet certificate refresh failed");
                true
            }
        };
        let observation = Observation {
            state,
            certificate_error,
            expires_at_ms: self.expires_at_ms,
        };
        self.status.send_replace(observation.report(
            &self.dns_name,
            &self.url,
            self.public_url_matches,
            piqueld_store::now_ms(),
        ));
    }
}

/// One refresh of the node's login state and certificate.
struct Observation {
    /// The node's backend state, such as `Running`.
    state: String,
    /// Whether reading the certificate failed.
    certificate_error: bool,
    /// When the node's certificate expires, in Unix milliseconds.
    expires_at_ms: i64,
}

impl Observation {
    /// Builds the status to publish at `now_ms`, listing what is wrong with the node.
    fn report(
        self,
        dns_name: &str,
        url: &str,
        public_url_matches: bool,
        now_ms: i64,
    ) -> TailnetStatus {
        let mut problems = Vec::new();
        if self.state != "Running" {
            problems.push(format!("Tailscale is {}", self.state));
        }
        if self.expires_at_ms <= now_ms {
            problems.push("the HTTPS certificate has expired".into());
        }
        if self.certificate_error {
            problems.push("certificate refresh failed; see daemon logs".into());
        }
        let healthy = problems.is_empty();
        if !public_url_matches {
            problems.push(format!(
                "auth.public_url is not {url}, so passkeys do not work on this origin"
            ));
        }
        TailnetStatus {
            enabled: true,
            healthy,
            state: self.state,
            dns_name: Some(dns_name.to_owned()),
            certificate_expires_at_ms: Some(self.expires_at_ms),
            public_url_matches,
            message: if problems.is_empty() {
                format!("Serving HTTPS at {url}")
            } else {
                problems.join("; ")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_refresh_does_not_block_shutdown_or_child_exit() {
        for child_exits in [false, true] {
            let mut daemon = Command::new("sh")
                .args(["-c", "read ignored"])
                .stdin(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn succeeds");
            let stdin = daemon.stdin.take().expect("the counter is present");
            let cancellation = CancellationToken::new();
            let refresh_dropped = CancellationToken::new();
            let guard = refresh_dropped.clone().drop_guard();
            let (started, running) = tokio::sync::oneshot::channel();
            let supervisor = tokio::spawn(Monitor::supervise(
                daemon,
                cancellation.clone(),
                async move {
                    let _guard = guard;
                    started.send(()).expect("the request is sent");
                    std::future::pending::<()>().await;
                },
            ));
            running.await.expect("the task joins");
            if child_exits {
                drop(stdin);
            } else {
                cancellation.cancel();
            }
            let result = tokio::time::timeout(Duration::from_secs(5), supervisor)
                .await
                .expect("a stalled refresh must not block supervision")
                .expect("the supervisor returns");
            if child_exits {
                assert!(
                    result
                        .expect_err("the supervisor fails")
                        .to_string()
                        .contains("tailscaled exited unexpectedly")
                );
            } else {
                result.expect("the supervisor succeeds");
            }
            assert!(cancellation.is_cancelled());
            assert!(refresh_dropped.is_cancelled());
        }
    }

    fn observe(state: &str, certificate_error: bool, expires_at_ms: i64) -> Observation {
        Observation {
            state: state.into(),
            certificate_error,
            expires_at_ms,
        }
    }

    /// A running node with a valid certificate is healthy, even when the
    /// public URL is another origin, which is only reported; login and
    /// certificate problems make it unhealthy.
    #[test]
    fn reports_follow_login_certificate_and_public_url() {
        let url = "https://piqueld.tail.ts.net:8443";
        for (observation, public_url_matches, healthy, message) in [
            (
                observe("Running", false, 2_000),
                true,
                true,
                "Serving HTTPS at https://piqueld.tail.ts.net:8443",
            ),
            (observe("Running", false, 2_000), false, true, url),
            (
                observe("NeedsLogin", false, 2_000),
                true,
                false,
                "Tailscale is NeedsLogin",
            ),
            (
                observe("Running", false, 1_000),
                true,
                false,
                "certificate has expired",
            ),
            (
                observe("Running", true, 2_000),
                true,
                false,
                "refresh failed",
            ),
        ] {
            let status = observation.report("piqueld.tail.ts.net", url, public_url_matches, 1_000);
            assert!(status.enabled);
            assert_eq!(status.healthy, healthy, "{}", status.message);
            assert_eq!(status.public_url_matches, public_url_matches);
            assert!(status.message.contains(message), "{}", status.message);
        }
        let status =
            observe("Running", false, 2_000).report("piqueld.tail.ts.net", url, true, 1_000);
        assert_eq!(status.certificate_expires_at_ms, Some(2_000));
    }
}
