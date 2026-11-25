//! Loopback listener for connections forwarded by tailscaled. Each connection
//! starts with a PROXY protocol v2 header carrying the client's tailnet address.
//!
//! Any local process can connect to a loopback port and write such a header,
//! so connections are accepted only from processes running as the daemon's
//! own user, like the `tailscaled` it starts. Those could already read the
//! daemon's database; anyone else could otherwise pose as any tailnet peer.

use axum::serve::Listener;
use std::{
    io::{Error, ErrorKind},
    net::{IpAddr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

/// tailscaled writes the header as soon as it connects. The owner check runs
/// within the same time.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections queued between header parsing and the HTTP server.
const QUEUE: usize = 64;
/// Accepted connections whose owner and header are checked at once.
const CHECKING: usize = 64;
const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Version 2 with the PROXY command; `LOCAL` health checks carry no client.
const PROXY_COMMAND: u8 = 0x21;
const TCP_V4: u8 = 0x11;
const TCP_V6: u8 = 0x21;

/// Forwarded connections paired with their tailnet client addresses.
pub struct ProxyListener {
    connections: mpsc::Receiver<(TcpStream, SocketAddr)>,
    address: SocketAddr,
}

impl ProxyListener {
    /// Reads headers from connections on `listener`, bound to `address`, until
    /// cancellation.
    /// Connections with a missing or malformed header are dropped.
    pub fn spawn(
        listener: TcpListener,
        address: SocketAddr,
        cancellation: CancellationToken,
    ) -> Self {
        let (sender, connections) = mpsc::channel(QUEUE);
        tokio::spawn(Self::accept(listener, address, sender, cancellation));
        Self {
            connections,
            address,
        }
    }

    async fn accept(
        mut listener: TcpListener,
        address: SocketAddr,
        connections: mpsc::Sender<(TcpStream, SocketAddr)>,
        cancellation: CancellationToken,
    ) {
        let daemon = rustix::process::geteuid().as_raw();
        let permits = Arc::new(Semaphore::new(CHECKING));
        loop {
            let accepted = tokio::select! {
                () = cancellation.cancelled() => return,
                accepted = async {
                    let permit = Arc::clone(&permits).acquire_owned().await;
                    (permit, Listener::accept(&mut listener).await)
                } => accepted,
            };
            let (Ok(permit), (mut stream, local)) = accepted else {
                return;
            };
            let connections = connections.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let checked = tokio::time::timeout(
                    HEADER_TIMEOUT,
                    Self::checked_client(&mut stream, local, address, daemon),
                );
                match checked.await {
                    Ok(Some(client)) => {
                        #[expect(
                            clippy::let_underscore_must_use,
                            reason = "a closed queue means the server is shutting down"
                        )]
                        let _ = connections.send((stream, client)).await;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        tracing::debug!(%local, "dropping a forwarded tailnet connection without a header");
                    }
                }
            });
        }
    }

    /// The client address of a connection from `local` to `listener`, if the
    /// daemon's user opened it and it starts with a valid header. Refusals
    /// are logged.
    async fn checked_client(
        stream: &mut TcpStream,
        local: SocketAddr,
        listener: SocketAddr,
        daemon: u32,
    ) -> Option<SocketAddr> {
        let owner = Self::owner(local, listener).await;
        if owner != Some(daemon) {
            tracing::warn!(%local, ?owner, "refusing a forwarded tailnet connection from another user");
            return None;
        }
        Self::client(stream)
            .await
            .inspect_err(|error| {
                tracing::debug!(%local, %error, "dropping forwarded tailnet connection");
            })
            .ok()
    }

    /// The PROXY v2 header announcing a TCP over IPv4 connection from
    /// `client` to `destination`, as tailscaled writes it.
    ///
    /// ```text
    /// [signature: 12][0x21 PROXY][0x11 TCP over IPv4][length: 12]
    /// [client ip: 4][destination ip: 4][client port: 2][destination port: 2]
    /// ```
    pub(crate) fn header(client: SocketAddrV4, destination: SocketAddrV4) -> Vec<u8> {
        let mut header = SIGNATURE.to_vec();
        header.extend([PROXY_COMMAND, TCP_V4, 0, 12]);
        header.extend(client.ip().octets());
        header.extend(destination.ip().octets());
        header.extend(client.port().to_be_bytes());
        header.extend(destination.port().to_be_bytes());
        header
    }

    /// Reads a PROXY v2 header and returns the client address it carries.
    async fn client(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<SocketAddr> {
        let invalid = |message| Error::new(ErrorKind::InvalidData, message);
        let mut header = [0; 16];
        stream.read_exact(&mut header).await?;
        if header[..12] != SIGNATURE || header[12] != PROXY_COMMAND {
            return Err(invalid("missing PROXY v2 header"));
        }
        let mut addresses = vec![0; usize::from(u16::from_be_bytes([header[14], header[15]]))];
        stream.read_exact(&mut addresses).await?;
        match header[13] {
            TCP_V4 => Self::source::<4>(&addresses),
            TCP_V6 => Self::source::<16>(&addresses),
            _ => None,
        }
        .ok_or_else(|| invalid("PROXY header has no TCP over IPv4 or IPv6 addresses"))
    }

    /// Extracts the source from an address block laid out as source address,
    /// destination address, source port, destination port.
    fn source<const N: usize>(addresses: &[u8]) -> Option<SocketAddr>
    where
        IpAddr: From<[u8; N]>,
    {
        let (source, rest) = addresses.split_first_chunk::<N>()?;
        let port = rest.get(N..)?.first_chunk::<2>()?;
        Some(SocketAddr::new(
            IpAddr::from(*source),
            u16::from_be_bytes(*port),
        ))
    }

    /// The user owning the local end of the loopback TCP connection from
    /// `peer` to `listener`, read from `/proc/net/tcp` or `/proc/net/tcp6`.
    async fn owner(peer: SocketAddr, listener: SocketAddr) -> Option<u32> {
        let table = if peer.is_ipv4() {
            "/proc/net/tcp"
        } else {
            "/proc/net/tcp6"
        };
        let table = tokio::fs::read_to_string(table).await.ok()?;
        Self::owner_in(&table, peer, listener)
    }

    /// Finds the open socket bound to `local` and connected to `remote` in a
    /// `/proc/net/tcp`-style table and returns its owner's user ID.
    ///
    /// Only established sockets still held by a process (nonzero inode)
    /// count: once a client closes, the kernel keeps its row with owner 0,
    /// which would otherwise vouch for whatever the client queued before
    /// closing.
    fn owner_in(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
        const ESTABLISHED: &str = "01";
        let (local, remote) = (Self::proc_address(local), Self::proc_address(remote));
        table.lines().skip(1).find_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            let open = columns.get(1) == Some(&local.as_str())
                && columns.get(2) == Some(&remote.as_str())
                && columns.get(3) == Some(&ESTABLISHED)
                && columns.get(9).is_some_and(|inode| *inode != "0");
            open.then(|| columns.get(7)?.parse().ok()).flatten()
        })
    }

    /// An address as the kernel prints it in `/proc/net/tcp`: each 32-bit
    /// word of the address in native byte order, then the port, all in
    /// uppercase hex.
    fn proc_address(address: SocketAddr) -> String {
        let octets = match address.ip() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let words: Vec<u8> = octets
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|word| u32::from_ne_bytes(*word).to_be_bytes())
            .collect();
        format!("{}:{:04X}", hex::encode_upper(words), address.port())
    }
}

impl Listener for ProxyListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.connections.recv().await {
            Some(connection) => connection,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.address)
    }
}

#[cfg(test)]
mod tests;
