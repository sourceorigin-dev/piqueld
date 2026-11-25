use super::{BollardDocker, StatusCode, WireError};

struct EngineStub {
    docker: BollardDocker,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

/// A client of the engine socket in a new directory, and the listener
/// behind it.
fn listen() -> (tempfile::TempDir, tokio::net::UnixListener, BollardDocker) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let docker = BollardDocker::connect(&socket).unwrap();
    (directory, listener, docker)
}

impl EngineStub {
    /// Serves one raw 200 response whose declared length may disagree with the body.
    fn respond(body: &'static str, declared_length: usize) -> Self {
        Self::serve(vec![(
            None,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{body}"
            ),
        )])
    }

    /// Serves JSON responses in order, asserting each request line.
    fn sequence(responses: Vec<(String, u16, serde_json::Value)>) -> Self {
        Self::serve(
            responses
                .into_iter()
                .map(|(request, status, body)| {
                    let body = body.to_string();
                    (
                        Some(request),
                        format!(
                            "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                    )
                })
                .collect(),
        )
    }

    /// Answers one connection per exchange with its raw response, checking
    /// the request line when one is expected.
    fn serve(exchanges: Vec<(Option<String>, String)>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (directory, listener, docker) = listen();
        let task = tokio::spawn(async move {
            for (expected, response) in exchanges {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(stream.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(headers).unwrap();
                if let Some(expected) = expected {
                    assert_eq!(
                        headers.lines().next().unwrap(),
                        format!("{expected} HTTP/1.1")
                    );
                }
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or_default();
                stream.read_exact(&mut vec![0; length]).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        Self {
            docker,
            task,
            _directory: directory,
        }
    }
}

#[tokio::test]
async fn service_update_retry_uses_id_and_rechecks_ownership() {
    use piqueld_core::resource::{
        APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, SERVICE_LABEL, SPEC_HASH_LABEL,
    };
    let app = piqueld_core::EnvironmentId::parse("app-update-race").unwrap();
    let name =
        piqueld_core::docker_resource_name(&app, piqueld_core::ResourceKind::Service, Some("web"));
    let spec = bollard::models::ServiceSpec {
        name: Some(name.clone()),
        labels: Some(std::collections::HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (INSTANCE_LABEL.into(), "test-instance".into()),
            (APPLICATION_LABEL.into(), app.to_string()),
            (SERVICE_LABEL.into(), "web".into()),
            (SPEC_HASH_LABEL.into(), format!("sha256:{}", "a".repeat(64))),
        ])),
        ..Default::default()
    };
    for changed in ["none", "owner", "id", "name", "removed"] {
        let mut refreshed =
            serde_json::json!({"ID":"immutable-id", "Version":{"Index":42}, "Spec":spec});
        match changed {
            "owner" => refreshed["Spec"]["Labels"][INSTANCE_LABEL] = "another-instance".into(),
            "id" => refreshed["ID"] = "replacement-id".into(),
            "name" => refreshed["Spec"]["Name"] = "renamed-service".into(),
            _ => {}
        }
        let mut responses = vec![
            (
                "POST /services/immutable-id/update?version=1&registryAuthFrom=spec".into(),
                500,
                serde_json::json!({"message":"rpc error: code = Unknown desc = update out of sequence"}),
            ),
            (
                "GET /services/immutable-id".into(),
                if changed == "removed" { 404 } else { 200 },
                refreshed,
            ),
        ];
        if changed == "none" {
            responses.push((
                "POST /services/immutable-id/update?version=42&registryAuthFrom=spec".into(),
                200,
                serde_json::json!({}),
            ));
        }
        let stub = EngineStub::sequence(responses);
        let result = stub
            .docker
            .update_service_wire("immutable-id", 1, &spec)
            .await;
        if changed == "none" {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(super::DockerError::OwnershipConflict)),
                "{changed}: {result:?}"
            );
        }
        stub.task.await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_service_request_closes_connection_driver() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_directory, listener, docker) = listen();
    let request = tokio::spawn(async move { docker.inspect_service_wire("test").await });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(stream.read_u8().await.unwrap());
    }
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{")
        .await
        .unwrap();
    tokio::task::yield_now().await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let mut byte = [0];
    let read =
        tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut byte)).await;
    assert_eq!(read.expect("cancelled connection must close").unwrap(), 0);
}

#[tokio::test]
async fn missing_engine_retains_io_cause_and_public_classification() {
    use std::error::Error;
    let (directory, listener, docker) = listen();
    drop(listener);
    std::fs::remove_file(directory.path().join("docker.sock")).unwrap();
    let error = docker.inspect_service_wire("test").await.unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    assert!(
        matches!(error, super::DockerError::UnavailableSource { .. }),
        "{error:?}"
    );
}

/// A malformed or truncated response fails the request with its decode
/// or transport cause.
#[tokio::test]
async fn bad_service_responses_retain_their_cause() {
    use std::error::Error;
    for (body, declared_length, decoding) in [("not JSON", 8, true), ("{", 20, false)] {
        let engine = EngineStub::respond(body, declared_length);
        let error = engine
            .docker
            .inspect_service_wire("test")
            .await
            .unwrap_err();
        engine.task.await.unwrap();
        let source = error.source().unwrap();
        if decoding {
            assert!(source.is::<serde_json::Error>(), "{error:?}");
            assert_eq!(
                error.to_string(),
                "Docker request failed while decode service response"
            );
        } else {
            assert!(source.is::<hyper::Error>(), "{error:?}");
        }
        assert!(
            matches!(error, super::DockerError::RequestSource { .. }),
            "{error:?}"
        );
    }
}

/// A secret that another request created, then removed before the
/// recheck, fails the request: Docker answered, so it is no outage.
#[tokio::test]
async fn a_secret_removed_during_creation_is_a_request_failure() {
    let response = |status: u16| {
        let body = r#"{"message":"fixture"}"#;
        (
            None,
            format!(
                "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    };
    let stub = EngineStub::serve(vec![response(404), response(409), response(404)]);
    let error = stub
        .docker
        .provision_secret("secret", b"value", &std::collections::BTreeMap::new())
        .await
        .unwrap_err();
    stub.task.await.unwrap();
    assert!(
        matches!(
            error,
            super::DockerError::Request(_) | super::DockerError::RequestSource { .. }
        ),
        "{error:?}"
    );
}

/// Both of Docker's answers to a start over a taken host port are
/// recognized; other failures are not.
#[test]
fn a_taken_host_port_is_recognized_in_either_wording() {
    let response = |message: &str| WireError::Response {
        path: "/containers/gateway/start".into(),
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: serde_json::json!({ "message": message })
            .to_string()
            .into_bytes(),
    };
    assert!(response("driver failed programming external connectivity on endpoint gateway: Bind for 0.0.0.0:80 failed: port is already allocated").port_in_use());
    assert!(
        response(
            "failed to listen on TCP socket: listen tcp4 0.0.0.0:443: bind: address already in use"
        )
        .port_in_use()
    );
    assert!(!response("container gateway is not running").port_in_use());
    assert!(!WireError::Public(super::DockerError::Request("start container")).port_in_use());
}

#[test]
fn update_retry_requires_the_exact_transient_daemon_response() {
    let transient = WireError::Response {
        path: "/services/web/update".into(),
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence"}"#.to_vec(),
    };
    let different_message = WireError::Response {
        path: "/services/web/update".into(),
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence: other"}"#
            .to_vec(),
    };
    let different_status = WireError::Response {
        path: "/services/web/update".into(),
        status: StatusCode::CONFLICT,
        body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence"}"#.to_vec(),
    };

    assert!(BollardDocker::update_out_of_sequence(&transient));
    assert!(!BollardDocker::update_out_of_sequence(&different_message));
    assert!(!BollardDocker::update_out_of_sequence(&different_status));
}

#[test]
fn healthcheck_key_is_translated_at_the_docker_wire_boundary() {
    let mut value = serde_json::json!({
        "TaskTemplate": {"ContainerSpec": {"HealthCheck": {"Test": ["CMD", "true"]}}}
    });
    BollardDocker::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
    let container = &value["TaskTemplate"]["ContainerSpec"];
    assert!(container.get("HealthCheck").is_none());
    assert!(container.get("Healthcheck").is_some());

    let mut response = serde_json::json!({"Spec": value});
    BollardDocker::rename_swarm_healthcheck(&mut response, "Healthcheck", "HealthCheck");
    let container = &response["Spec"]["TaskTemplate"]["ContainerSpec"];
    assert!(container.get("HealthCheck").is_some());
    assert!(container.get("Healthcheck").is_none());
}
