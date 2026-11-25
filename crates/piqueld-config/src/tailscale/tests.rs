use super::*;

/// The node's status as `tailscaled` 1.104 reports it, trimmed.
const RUNNING: &str = r#"{"Version":"1.104.6","BackendState":"Running","AuthURL":"",
    "TailscaleIPs":["100.123.77.68","fd7a:115c:a1e0::c538:4d44"],
    "Self":{"HostName":"piqueld","DNSName":"piqueld.tail1234.ts.net.",
    "TailscaleIPs":["100.123.77.68","fd7a:115c:a1e0::c538:4d44"]}}"#;

#[tokio::test]
async fn status_reads_the_node_without_its_peers() {
    let fake = FakeLocalApi::serve(&[("/localapi/v0/status?peers=false", 200, RUNNING)]).unwrap();
    let status = fake.api().status().await.unwrap();
    assert!(status.running());
    assert_eq!(status.dns_name().unwrap(), "piqueld.tail1234.ts.net");
    assert_eq!(status.addresses().count(), 2);

    let fake = FakeLocalApi::serve(&[(
        "/localapi/v0/status?peers=false",
        200,
        r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/1","Self":{"DNSName":"","TailscaleIPs":null}}"#,
    )]).unwrap();
    let status = fake.api().status().await.unwrap();
    assert!(!status.running());
    assert_eq!(status.auth_url, "https://login.tailscale.com/a/1");
    assert_eq!(status.dns_name(), None);
    assert_eq!(status.addresses().count(), 0);
}

#[tokio::test]
async fn whois_reads_users_and_treats_tagged_nodes_as_userless() {
    let fake = FakeLocalApi::serve(&[
        (
            "/localapi/v0/whois?addr=100.64.0.1",
            200,
            r#"{"Node":{"Name":"laptop.tail1234.ts.net.","Tags":null},
            "UserProfile":{"LoginName":"alice@example.com"}}"#,
        ),
        (
            "/localapi/v0/whois?addr=fd7a:115c:a1e0::2",
            200,
            r#"{"Node":{"Name":"runner.tail1234.ts.net.","Tags":["tag:ci"]},
            "UserProfile":{"LoginName":"tagged-devices"}}"#,
        ),
        (
            "/localapi/v0/whois?addr=100.64.0.9",
            404,
            "no match for IP:port\n",
        ),
    ])
    .unwrap();
    let api = fake.api();
    let laptop = api.whois("100.64.0.1".parse().unwrap()).await.unwrap();
    assert_eq!(laptop.login.as_deref(), Some("alice@example.com"));
    assert_eq!(laptop.node, "laptop");
    let runner = api
        .whois("fd7a:115c:a1e0::2".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(runner.login, None);
    assert_eq!(runner.tags, ["tag:ci"]);
    let unknown = api.whois("100.64.0.9".parse().unwrap()).await.unwrap_err();
    assert!(format!("{unknown:#}").contains("HTTP 404"), "{unknown:#}");
    assert!(format!("{unknown:#}").contains("no match"), "{unknown:#}");
}

#[tokio::test]
async fn certificates_are_read_as_pem_and_refusals_are_errors() {
    let fake = FakeLocalApi::serve(&[
        (
            "/localapi/v0/cert/piqueld.tail1234.ts.net?type=cert",
            200,
            "-----BEGIN CERTIFICATE-----\n",
        ),
        (
            "/localapi/v0/cert/other.tail1234.ts.net?type=cert",
            500,
            "invalid domain \"other.tail1234.ts.net\"\n",
        ),
    ])
    .unwrap();
    let api = fake.api();
    let pem = api.certificate("piqueld.tail1234.ts.net").await.unwrap();
    assert_eq!(pem, b"-----BEGIN CERTIFICATE-----\n");
    let refused = api.certificate("other.tail1234.ts.net").await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("invalid domain"),
        "{refused:#}"
    );
}

#[tokio::test]
async fn serve_config_reads_forwards_and_nothing_served() {
    let fake = FakeLocalApi::serve(&[(
        "/localapi/v0/serve-config",
        200,
        r#"{"TCP":{"7001":{"TCPForward":"127.0.0.1:4000","TerminateTLS":"node.ts.net","ProxyProtocol":2},
        "443":{"HTTPS":true}},"Web":{}}"#,
    )]).unwrap();
    let config = fake.api().serve_config().await.unwrap();
    assert_eq!(
        config.port(7001),
        PortUse::Forwarded("127.0.0.1:4000".parse().unwrap())
    );
    assert_eq!(config.port(443), PortUse::Other);
    assert_eq!(config.port(7002), PortUse::Unserved);
    for empty in ["null", "{}"] {
        let fake = FakeLocalApi::serve(&[("/localapi/v0/serve-config", 200, empty)]).unwrap();
        let config = fake.api().serve_config().await.unwrap();
        assert_eq!(config.port(443), PortUse::Unserved, "{empty}");
    }
}

/// Without a `tailscaled`, or with one that stopped and left its socket,
/// every read fails naming the socket.
#[tokio::test]
async fn reads_fail_when_tailscaled_is_down() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.sock");
    let stale = directory.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    for socket in [missing, stale] {
        let api = LocalApi::new(socket.clone(), Duration::from_secs(5));
        let error = format!("{:#}", api.status().await.unwrap_err());
        assert!(error.contains(&socket.display().to_string()), "{error}");
        assert!(error.contains("connect"), "{error}");
    }
}
