//! The daemon's own tailnet node. It serves the website over HTTPS with the
//! tailnet-issued certificate, independently of the host's Tailscale daemon.
//!
//! piqueld supervises a dedicated `tailscaled` in userspace-networking mode, or
//! shares a configured one. It reads the node's state through the `LocalAPI`
//! (see [`piqueld_config::tailscale`]), and runs the `tailscale` CLI to log the
//! node in and change its Serve configuration, which the `LocalAPI` can't do
//! as simply. So the CLI (and `tailscaled`, unless shared) must be on `PATH`.

mod node;
mod proxy;
mod whois;

pub use node::{Node, NodeListener};
pub use proxy::ProxyListener;
pub use whois::{LookupTimedOut, TailnetLookup, Whois};

use anyhow::{Context, Result, bail, ensure};
use piqueld_config::tailscale::{LocalApi, PortUse};
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
    async fn check_takeover(port: u16, serving: PortUse) -> Result<()> {
        match serving {
            PortUse::Unserved => Ok(()),
            PortUse::Forwarded(target) => {
                let in_use = format!(
                    "tailnet HTTPS port {port} already forwards to {target}; stop that process or set another tailscale.https_port"
                );
                match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
                    Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
                        tracing::info!(port, %target, "replacing the tailnet HTTPS port's stale forward");
                        Ok(())
                    }
                    Ok(Ok(_)) => bail!("{in_use} (it still accepts connections)"),
                    Ok(Err(error)) => Err(anyhow::Error::new(error).context(in_use)),
                    Err(elapsed) => Err(anyhow::Error::new(elapsed).context(in_use)),
                }
            }
            PortUse::Other => bail!(
                "tailnet HTTPS port {port} is already in the node's Serve configuration; remove it or set another tailscale.https_port"
            ),
        }
    }

    /// How the node serves `port`.
    async fn serving(&self, port: u16) -> Result<PortUse> {
        Ok(self.api.serve_config().await?.port(port))
    }

    /// Runs `tailscale <args>` to completion and returns its stdout. Stderr is
    /// logged as it arrives, which is how interactive login URLs from `up`
    /// reach the operator, and is included in the error on failure. Only `up`
    /// may wait indefinitely for interactive approval.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let operation = args.first().copied().unwrap_or_default();
        Self::run_command(
            Command::new("tailscale")
                .arg(format!("--socket={}", self.api.socket().display()))
                .args(args),
            operation,
        )
        .await
    }

    async fn run_command(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
        let output = Self::output(command, operation);
        if operation == "up" {
            output.await
        } else {
            tokio::time::timeout(COMMAND_TIMEOUT, output)
                .await
                .with_context(|| {
                    format!("tailscale {operation} timed out after {COMMAND_TIMEOUT:?}")
                })?
        }
    }

    /// Dropping this future on timeout or shutdown kills the CLI child.
    async fn output(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to run tailscale")?;
        let mut stdout = child.stdout.take().context("capture tailscale stdout")?;
        let stderr = child.stderr.take().context("capture tailscale stderr")?;
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
        .with_context(|| format!("failed to read tailscale {operation} output"))?;
        let status = child.wait().await.context("failed to wait for tailscale")?;
        ensure!(
            status.success(),
            "tailscale {operation} failed ({status}): {}",
            stderr.trim()
        );
        Ok(stdout)
    }

    /// Fetches the certificate for the node's name and returns its expiry.
    /// tailscaled caches it in the node state and renews it before expiry.
    async fn certificate_expiry_ms(&self, dns_name: &str) -> Result<i64> {
        let pem = self
            .api
            .certificate(dns_name)
            .await
            .context("failed to obtain the tailnet HTTPS certificate; enable HTTPS certificates for the tailnet")?;
        let validity = crate::certificate::Validity::of_leaf(&pem)
            .map_err(|error| anyhow::anyhow!("invalid {error}"))?;
        Ok(validity.not_after_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_config::tailscale::{FakeLocalApi, ServeConfig};

    #[tokio::test(start_paused = true)]
    async fn noninteractive_commands_time_out_but_login_can_wait() {
        for operation in ["serve", "up"] {
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 600"]);
            let result = tokio::time::timeout(
                COMMAND_TIMEOUT * 2,
                Tailscaled::run_command(&mut command, operation),
            )
            .await;
            if operation == "up" {
                assert!(result.is_err(), "interactive login must keep waiting");
            } else {
                let error = result.unwrap().unwrap_err().to_string();
                assert!(
                    error.contains(&format!("tailscale {operation} timed out")),
                    "{error}"
                );
            }
        }
    }

    /// The node's certificate expiry comes from the `LocalAPI`'s PEM, and a
    /// refused certificate explains how to enable them.
    #[tokio::test]
    async fn certificate_expiry_is_read_from_the_local_api() {
        let certificate = {
            use openssl::{asn1::Asn1Time, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509};
            let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
            let mut builder = x509::X509Builder::new().unwrap();
            builder.set_pubkey(&key).unwrap();
            builder
                .set_not_before(&Asn1Time::from_unix(1_700_000_000).unwrap())
                .unwrap();
            builder
                .set_not_after(&Asn1Time::from_unix(1_800_000_000).unwrap())
                .unwrap();
            builder.sign(&key, MessageDigest::sha256()).unwrap();
            String::from_utf8(builder.build().to_pem().unwrap()).unwrap()
        };
        let fake = FakeLocalApi::serve(&[
            (
                "/localapi/v0/cert/piqueld.tail1234.ts.net?type=cert",
                200,
                &certificate,
            ),
            (
                "/localapi/v0/cert/other.tail1234.ts.net?type=cert",
                500,
                "invalid domain \"other.tail1234.ts.net\"; must be one of [\"piqueld.tail1234.ts.net\"]\n",
            ),
        ]).unwrap();
        let tailscaled = Tailscaled::at(fake.socket().to_owned());
        let expiry = tailscaled
            .certificate_expiry_ms("piqueld.tail1234.ts.net")
            .await
            .unwrap();
        assert_eq!(expiry, 1_800_000_000_000);
        let refused = tailscaled
            .certificate_expiry_ms("other.tail1234.ts.net")
            .await
            .unwrap_err();
        let refused = format!("{refused:#}");
        assert!(
            refused.starts_with("failed to obtain the tailnet HTTPS certificate"),
            "{refused}"
        );
        assert!(refused.contains("HTTP 500"), "{refused}");
        assert!(refused.contains("invalid domain"), "{refused}");
    }

    /// A port forwarding to a live listener is refused, and one whose
    /// listener refuses connections is taken over, as is an unserved port. A
    /// port whose owner can't be reached, or served any other way, is
    /// refused.
    #[tokio::test]
    async fn only_unserved_or_stale_ports_are_taken_over() {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let listener = bind().await.unwrap();
        let live = listener.local_addr().unwrap();
        // Its daemon is gone: nothing listens there any more.
        let stale = bind().await.unwrap().local_addr().unwrap();
        let config = format!(
            r#"{{"TCP":{{
                "7001":{{"TCPForward":"{live}","TerminateTLS":"node.ts.net","ProxyProtocol":2}},
                "7002":{{"TCPForward":"{stale}"}},
                "443":{{"HTTPS":true}}
            }},"Web":{{}}}}"#
        );
        let fake = FakeLocalApi::serve(&[("/localapi/v0/serve-config", 200, &config)]).unwrap();
        let tailscaled = &Tailscaled::at(fake.socket().to_owned());
        let check = |port| async move {
            Tailscaled::check_takeover(port, tailscaled.serving(port).await.unwrap()).await
        };
        assert_eq!(
            tailscaled.serving(7001).await.unwrap(),
            PortUse::Forwarded(live)
        );
        let refused = check(7001).await.unwrap_err().to_string();
        assert!(refused.contains("still accepts connections"), "{refused}");
        check(7002).await.unwrap();
        // TEST-NET-1 never answers: a timeout or an unreachable network.
        let unreachable: ServeConfig =
            serde_json::from_str(r#"{"TCP":{"7004":{"TCPForward":"192.0.2.1:7"}}}"#).unwrap();
        let refused = Tailscaled::check_takeover(7004, unreachable.port(7004))
            .await
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("192.0.2.1:7"),
            "{refused:#}"
        );
        check(7003).await.unwrap();
        check(443).await.unwrap_err();
    }
}
