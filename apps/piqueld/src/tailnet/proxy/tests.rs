use super::*;
use std::net::Ipv6Addr;

fn header(family: u8, addresses: &[u8]) -> Vec<u8> {
    let mut header = SIGNATURE.to_vec();
    header.extend([PROXY_COMMAND, family]);
    header.extend(u16::try_from(addresses.len()).unwrap().to_be_bytes());
    header.extend(addresses);
    header
}

/// The header the relays write for `100.64.0.1:40000` connecting to
/// `100.64.0.2:443`.
fn v4_header() -> Vec<u8> {
    ProxyListener::header(
        "100.64.0.1:40000".parse().unwrap(),
        "100.64.0.2:443".parse().unwrap(),
    )
}

#[tokio::test]
async fn reads_ipv4_and_ipv6_clients_and_leaves_the_payload() {
    assert_eq!(
        v4_header(),
        header(
            TCP_V4,
            &[100, 64, 0, 1, 100, 64, 0, 2, 0x9c, 0x40, 0x01, 0xbb]
        )
    );
    let mut v4 = v4_header();
    v4.extend(b"GET /");
    let mut stream = v4.as_slice();
    assert_eq!(
        ProxyListener::client(&mut stream).await.unwrap(),
        "100.64.0.1:40000".parse().unwrap()
    );
    assert_eq!(stream, b"GET /");

    let client: Ipv6Addr = "fd7a:115c:a1e0::1".parse().unwrap();
    let v6 = [
        client.octets().as_slice(),
        &Ipv6Addr::LOCALHOST.octets(),
        &[0x9c, 0x40, 0x01, 0xbb],
        &[0x04, 0x00, 0x01, 0x00],
    ]
    .concat();
    assert_eq!(
        ProxyListener::client(&mut header(TCP_V6, &v6).as_slice())
            .await
            .unwrap(),
        SocketAddr::new(client.into(), 40000)
    );
}

/// The owner of a forwarded connection is read from the socket table, so
/// a connection from another user's process is told apart and refused, as
/// is one whose client has already closed.
#[test]
fn connections_are_attributed_to_their_owner() {
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let listener: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let line = |local: SocketAddr, remote: SocketAddr, state: &str, uid: u32, inode: u32| {
        let (local, remote) = (
            ProxyListener::proc_address(local),
            ProxyListener::proc_address(remote),
        );
        format!("   0: {local} {remote} {state} 0:0 0:0 0 {uid} 0 {inode} 1")
    };
    let table = |rows: [String; 2]| format!("  sl  header\n{}\n{}", rows[0], rows[1]);
    let open = table([
        line(listener, peer, "01", 1000, 7),
        line(peer, listener, "01", 1001, 8),
    ]);
    assert_eq!(ProxyListener::owner_in(&open, peer, listener), Some(1001));
    let elsewhere = "127.0.0.1:1".parse().unwrap();
    assert_eq!(ProxyListener::owner_in(&open, listener, elsewhere), None);
    let closed = table([
        line(listener, peer, "01", 1000, 7),
        line(peer, listener, "05", 0, 0),
    ]);
    assert_eq!(ProxyListener::owner_in(&closed, peer, listener), None);
    let loopback = if cfg!(target_endian = "little") {
        "0100007F"
    } else {
        "7F000001"
    };
    assert_eq!(
        ProxyListener::proc_address(peer),
        format!("{loopback}:9C40")
    );
}

/// Serves each request's client address through a [`ProxyListener`] on a
/// loopback port, until the token is cancelled.
async fn serve() -> (SocketAddr, CancellationToken) {
    use axum::{extract::ConnectInfo, routing::get, serve::ListenerExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let tap: fn(&mut TcpStream) = |_| {};
    let proxied = ProxyListener::spawn(listener, address, cancellation.clone()).tap_io(tap);
    let router = axum::Router::new().route(
        "/",
        get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
    );
    tokio::spawn(
        axum::serve(
            proxied,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    (address, cancellation)
}

/// Connects to `address` and sends a forwarded request, returning the stream
/// to read the response from.
async fn request(address: SocketAddr) -> TcpStream {
    use tokio::io::AsyncWriteExt;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&v4_header()).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: piqueld\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream
}

#[tokio::test]
async fn handlers_see_the_proxied_client_address() {
    let (address, cancellation) = serve().await;
    let mut response = String::new();
    request(address)
        .await
        .read_to_string(&mut response)
        .await
        .unwrap();
    cancellation.cancel();
    assert!(response.ends_with("100.64.0.1:40000"), "{response}");
}

/// Connections that never send a header hold at most [`CHECKING`] tasks:
/// later connections wait until one of them closes.
#[tokio::test]
async fn stalled_connections_hold_a_bounded_number_of_checks() {
    let (address, cancellation) = serve().await;
    let mut stalled = Vec::new();
    for _ in 0..CHECKING {
        stalled.push(TcpStream::connect(address).await.unwrap());
    }
    let mut waiting = request(address).await;
    let mut response = String::new();
    let early = tokio::time::timeout(
        Duration::from_millis(200),
        waiting.read_to_string(&mut response),
    );
    assert!(early.await.is_err(), "served past the limit: {response}");

    stalled.pop();
    waiting.read_to_string(&mut response).await.unwrap();
    cancellation.cancel();
    assert!(response.ends_with("100.64.0.1:40000"), "{response}");
}

#[tokio::test]
async fn rejects_missing_truncated_and_unsupported_headers() {
    let mut local = header(TCP_V4, &[0; 12]);
    local[12] = 0x20;
    for input in [
        b"GET / HTTP/1.1\r\nHost: piqueld\r\n\r\n".to_vec(),
        header(TCP_V4, &[0; 8]),
        header(0x00, &[]),
        local,
        header(TCP_V4, &[0; 12])[..20].to_vec(),
    ] {
        assert!(
            ProxyListener::client(&mut input.as_slice()).await.is_err(),
            "accepted {input:?}"
        );
    }
}
