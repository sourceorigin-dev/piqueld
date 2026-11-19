use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A registry on `port` holding `repository`.
fn registry(port: u16, path: &str) -> BuildRegistry {
    BuildRegistry::new(BuildRepository::try_from(format!("localhost:{port}/{path}")).unwrap())
        .unwrap()
}

/// Answers one request with `response`, returning the request line.
async fn answer(listener: &tokio::net::TcpListener, response: String) -> String {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = vec![0; 4096];
    let read = stream.read(&mut request).await.unwrap();
    stream.write_all(response.as_bytes()).await.unwrap();
    String::from_utf8_lossy(&request[..read])
        .lines()
        .next()
        .unwrap()
        .to_owned()
}

/// An HTTP response with `status` and, if any, a registry error.
fn response(status: &str, error: Option<(&str, &str)>) -> String {
    let body = error.map_or_else(String::new, |(code, message)| {
        format!(r#"{{"errors":[{{"code":"{code}","message":"{message}"}}]}}"#)
    });
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn deletes_by_digest_and_reports_why_the_registry_kept_an_image() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let piqueld = registry(port, "piqueld");
    let digest = piqueld_core::Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
    let image = piqueld.repository.image(&digest);
    let unknown = Some(("MANIFEST_UNKNOWN", "manifest unknown"));
    let disabled = Some(("UNSUPPORTED", "The operation is unsupported."));
    for (answered, refused) in [
        (response("202 Accepted", None), None),
        (response("404 Not Found", unknown), None),
        // A proxy's 404 says nothing of the image.
        (response("404 Not Found", None), Some("404 Not Found")),
        (
            response("405 Method Not Allowed", disabled),
            Some("405 Method Not Allowed: The operation is unsupported."),
        ),
    ] {
        let (request, result) = tokio::join!(answer(&listener, answered), piqueld.delete(&image));
        assert_eq!(
            request,
            format!("DELETE /v2/piqueld/manifests/{digest} HTTP/1.1")
        );
        match (result, refused) {
            (Ok(()), None) => {}
            (Err(error @ RegistryError::Refused { .. }), Some(reason)) => {
                assert!(error.to_string().ends_with(reason), "{error}");
            }
            (result, _) => panic!("unexpected {result:?}"),
        }
    }
    // An image of another repository is never sent.
    let other = registry(port, "other").repository.image(&digest);
    assert!(matches!(
        piqueld.delete(&other).await,
        Err(RegistryError::NotPushed(_))
    ));

    // Only a successful answer counts as available, not a proxy's 502.
    let (_, ok) = tokio::join!(answer(&listener, response("200 OK", None)), piqueld.check());
    ok.unwrap();
    let (_, bad_gateway) = tokio::join!(
        answer(&listener, response("502 Bad Gateway", None)),
        piqueld.check()
    );
    assert!(matches!(
        bad_gateway,
        Err(RegistryError::Unavailable { .. })
    ));

    // Once nothing listens, the registry is unreachable.
    drop(listener);
    assert!(matches!(
        piqueld.check().await,
        Err(RegistryError::Unreachable { .. })
    ));
    assert!(matches!(
        piqueld.delete(&image).await,
        Err(RegistryError::Unreachable { .. })
    ));
}

#[test]
fn only_a_registry_on_this_hosts_loopback_is_reached_over_http() {
    for (repository, base) in [
        ("localhost:5000/piqueld", "http://localhost:5000"),
        ("127.0.0.1:5000/piqueld", "http://127.0.0.1:5000"),
        (
            "registry.example.ts.net:5000/piqueld",
            "https://registry.example.ts.net:5000",
        ),
        ("registry.example/team/piqueld", "https://registry.example"),
    ] {
        let registry =
            BuildRegistry::new(BuildRepository::try_from(repository.to_owned()).unwrap()).unwrap();
        assert_eq!(registry.base(), base);
    }
}
