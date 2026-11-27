use super::*;
use piqueld_config::tailscale::{FakeLocalApi, ServeConfig};
use piqueld_core::ErrorChain as _;

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
            let error = result
                .expect("the command runs")
                .expect_err("the command fails")
                .to_string();
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
        let key = PKey::from_rsa(Rsa::generate(2048).expect("generate succeeds"))
            .expect("from rsa succeeds");
        let mut builder = x509::X509Builder::new().expect("the value is valid");
        builder.set_pubkey(&key).expect("set pubkey succeeds");
        builder
            .set_not_before(&Asn1Time::from_unix(1_700_000_000).expect("from unix succeeds"))
            .expect("set not before succeeds");
        builder
            .set_not_after(&Asn1Time::from_unix(1_800_000_000).expect("from unix succeeds"))
            .expect("set not after succeeds");
        builder
            .sign(&key, MessageDigest::sha256())
            .expect("sign succeeds");
        String::from_utf8(builder.build().to_pem().expect("to pem succeeds"))
            .expect("from utf8 succeeds")
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
    ]).expect("serve succeeds");
    let tailscaled = Tailscaled::at(fake.socket().to_owned());
    let expiry = tailscaled
        .certificate_expiry_ms("piqueld.tail1234.ts.net")
        .await
        .expect("certificate expiry ms succeeds");
    assert_eq!(expiry, 1_800_000_000_000);
    let refused = tailscaled
        .certificate_expiry_ms("other.tail1234.ts.net")
        .await
        .expect_err("certificate expiry ms fails");
    let refused = refused.chain();
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
    let listener = bind().await.expect("the listener binds");
    let live = listener.local_addr().expect("the listener has an address");
    let stale = bind()
        .await
        .expect("the listener binds")
        .local_addr()
        .expect("the listener has an address");
    let config = format!(
        r#"{{"TCP":{{
            "7001":{{"TCPForward":"{live}","TerminateTLS":"node.ts.net","ProxyProtocol":2}},
            "7002":{{"TCPForward":"{stale}"}},
            "443":{{"HTTPS":true}}
        }},"Web":{{}}}}"#
    );
    let fake = FakeLocalApi::serve(&[("/localapi/v0/serve-config", 200, &config)])
        .expect("serve succeeds");
    let tailscaled = &Tailscaled::at(fake.socket().to_owned());
    let check = |port| async move {
        Tailscaled::check_takeover(
            port,
            tailscaled.serving(port).await.expect("serving succeeds"),
        )
        .await
    };
    assert_eq!(
        tailscaled.serving(7001).await.expect("serving succeeds"),
        PortUse::Forwarded(live)
    );
    let refused = check(7001)
        .await
        .expect_err("the port check fails")
        .to_string();
    assert!(refused.contains("still accepts connections"), "{refused}");
    check(7002).await.expect("the port check succeeds");
    let unreachable: ServeConfig =
        serde_json::from_str(r#"{"TCP":{"7004":{"TCPForward":"192.0.2.1:7"}}}"#)
            .expect("the value deserializes");
    let refused = Tailscaled::check_takeover(7004, unreachable.port(7004))
        .await
        .expect_err("check takeover fails");
    assert!(
        refused.chain().contains("192.0.2.1:7"),
        "{}",
        refused.chain()
    );
    check(7003).await.expect("the port check succeeds");
    check(443).await.expect_err("the port check fails");
}
