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
    with_body(status, "", &body)
}

/// An HTTP response with `status`, extra `headers` (each ending in `\r\n`)
/// and `body`, closing its connection, so each request opens another.
fn with_body(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Answers one request after another with `responses`, returning their
/// request lines.
async fn answer_all(listener: &tokio::net::TcpListener, responses: Vec<String>) -> Vec<String> {
    let mut requests = Vec::new();
    for response in responses {
        requests.push(answer(listener, response).await);
    }
    requests
}

#[tokio::test]
async fn deletes_by_digest_and_reports_why_the_registry_kept_an_image() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let piqueld = registry(port, "piqueld");
    let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
    let image = piqueld.repository.image(&digest);
    let unknown = Some(("MANIFEST_UNKNOWN", "manifest unknown"));
    let disabled = Some(("UNSUPPORTED", "The operation is unsupported."));
    for (answered, refused) in [
        (response("202 Accepted", None), None),
        (response("404 Not Found", unknown), None),
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
    let other = registry(port, "other").repository.image(&digest);
    assert!(matches!(
        piqueld.delete(&other).await,
        Err(RegistryError::NotPushed(_))
    ));

    drop(listener);
    let owner = InstanceId::parse("instance-1").unwrap();
    assert!(matches!(
        piqueld.pushed_images(&owner).await,
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

#[tokio::test]
async fn lists_only_this_installations_tagged_images_page_by_page() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let piqueld = registry(port, "piqueld");
    let owner = InstanceId::parse("instance-1").unwrap();
    let digest = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
    let found = |byte: char| {
        with_body(
            "200 OK",
            &format!("docker-content-digest: {}\r\n", digest(byte)),
            "",
        )
    };
    let tags = |tags: &str| format!(r#"{{"name":"piqueld","tags":[{tags}]}}"#);
    let next = "link: <?last=0&n=1000>; rel=\"prev\"\r\n\
        link: <https://proxy.example/v2/piqueld/tags/list?last=instance-1.a&n=1000>; rel=\"next\"\r\n";
    let last = "link: <?last=instance-1.gone&n=1000>; rel=next\r\n";
    let (requests, images) = tokio::join!(
        answer_all(
            &listener,
            vec![
                with_body(
                    "200 OK",
                    next,
                    &tags(r#""instance-10.x","latest","instance-1.a""#)
                ),
                with_body("200 OK", last, &tags(r#""instance-1.b","instance-1.gone""#)),
                with_body("200 OK", "", &tags("")),
                found('a'),
                found('b'),
                with_body("404 Not Found", "", ""),
            ],
        ),
        piqueld.pushed_images(&owner)
    );
    assert_eq!(
        requests,
        [
            "GET /v2/piqueld/tags/list?n=1000 HTTP/1.1",
            "GET /v2/piqueld/tags/list?last=instance-1.a&n=1000 HTTP/1.1",
            "GET /v2/piqueld/tags/list?last=instance-1.gone&n=1000 HTTP/1.1",
            "HEAD /v2/piqueld/manifests/instance-1.a HTTP/1.1",
            "HEAD /v2/piqueld/manifests/instance-1.b HTTP/1.1",
            "HEAD /v2/piqueld/manifests/instance-1.gone HTTP/1.1",
        ]
    );
    let expected = ['a', 'b'].map(|byte| {
        piqueld
            .repository
            .image(&Sha256Digest::parse(digest(byte)).unwrap())
    });
    assert_eq!(images.unwrap(), expected.clone().into());

    let all = with_body(
        "200 OK",
        "",
        &tags(r#""instance-1.a","instance-1.b","instance-1.gone""#),
    );
    let missing = with_body("404 Not Found", "", "");
    let (requests, images) = tokio::join!(
        answer_all(&listener, vec![all.clone(), missing.clone()]),
        piqueld.pushed_images(&owner)
    );
    assert_eq!(
        requests,
        [
            "GET /v2/piqueld/tags/list?n=1000 HTTP/1.1",
            "HEAD /v2/piqueld/manifests/instance-1.gone HTTP/1.1",
        ]
    );
    assert_eq!(images.unwrap(), expected.clone().into());
    let (_, deleted) = tokio::join!(
        answer(&listener, response("202 Accepted", None)),
        piqueld.delete(&expected[0])
    );
    deleted.unwrap();
    let (requests, images) = tokio::join!(
        answer_all(&listener, vec![all, missing]),
        piqueld.pushed_images(&owner)
    );
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(images.unwrap(), [expected[1].clone()].into());

    let unknown = response(
        "404 Not Found",
        Some(("NAME_UNKNOWN", "repository name not known")),
    );
    let (_, images) = tokio::join!(answer(&listener, unknown), piqueld.pushed_images(&owner));
    assert!(images.unwrap().is_empty());
    let (_, images) = tokio::join!(
        answer(&listener, response("404 Not Found", None)),
        piqueld.pushed_images(&owner)
    );
    assert!(matches!(images, Err(RegistryError::Unavailable { .. })));
    let (_, images) = tokio::join!(
        answer(&listener, response("502 Bad Gateway", None)),
        piqueld.pushed_images(&owner)
    );
    assert!(matches!(images, Err(RegistryError::Unavailable { .. })));
}

#[test]
fn each_push_gets_a_new_tag_naming_the_installation() {
    let owner = InstanceId::parse("instance-1").unwrap();
    let (first, second) = (
        BuildRegistry::new_tag(&owner),
        BuildRegistry::new_tag(&owner),
    );
    assert_ne!(first, second);
    assert!(first.starts_with("instance-1.") && first.len() == "instance-1.".len() + 32);
}
