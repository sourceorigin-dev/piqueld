use super::*;
use std::{net::TcpListener, thread};

fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

/// Answers one connection on a free loopback port with `response`.
fn serve(response: &'static [u8]) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("the listener binds");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the connection is accepted");
        let mut request = Vec::new();
        let mut buffer = [0; 256];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("the request is read");
            assert_ne!(read, 0, "the request ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        for chunk in response.chunks(5) {
            stream.write_all(chunk).expect("the response is written");
        }
    });
    port
}

fn probe(port: u16) -> HttpProbe {
    HttpProbe {
        port,
        path: "/health/ready".into(),
        timeout_seconds: 1,
    }
}

#[test]
fn command_round_trips_through_parse() {
    let probe = probe(8080);
    let command = probe.command();
    assert_eq!(
        command,
        arguments(&[
            CONTAINER_PATH,
            "http",
            "--port",
            "8080",
            "--path",
            "/health/ready",
            "--timeout",
            "1"
        ])
    );
    assert_eq!(HttpProbe::parse(command.into_iter().skip(1)), Ok(probe));
}

#[test]
fn parse_rejects_malformed_arguments() {
    for (values, expected) in [
        (&["tcp"][..], ArgumentError::Usage),
        (
            &["http", "--port", "1", "--port", "2"],
            ArgumentError::Unexpected("--port".into()),
        ),
        (
            &["http", "--port"],
            ArgumentError::Unexpected("--port".into()),
        ),
        (
            &["http", "--port", "1", "--path", "/"],
            ArgumentError::Missing("--timeout"),
        ),
        (
            &["http", "--port", "0", "--path", "/", "--timeout", "1"],
            ArgumentError::Invalid {
                flag: "--port",
                value: "0".into(),
            },
        ),
        (
            &["http", "--port", "1", "--path", "/a b", "--timeout", "1"],
            ArgumentError::Invalid {
                flag: "--path",
                value: "/a b".into(),
            },
        ),
    ] {
        assert_eq!(HttpProbe::parse(arguments(values)), Err(expected));
    }
}

#[test]
fn success_and_redirect_statuses_are_healthy() {
    let port = serve(b"HTTP/1.1 204 No Content\r\n\r\n");
    assert_eq!(
        probe(port).check().expect("the probe reaches the server"),
        204
    );
    let port = serve(b"HTTP/1.0 302 Found\r\nLocation: /\r\n\r\n");
    assert_eq!(
        probe(port).check().expect("the probe reaches the server"),
        302
    );
    let port = serve(b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 200 OK\r\n\r\n");
    assert_eq!(
        probe(port).check().expect("the probe reaches the server"),
        200
    );
}

#[test]
fn failures_are_unhealthy() {
    let port = serve(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
    assert!(matches!(probe(port).check(), Err(ProbeError::Status(503))));
    let port = serve(b"SSH-2.0-OpenSSH\r\n");
    assert!(matches!(probe(port).check(), Err(ProbeError::Malformed(_))));
    let port = serve(b"HTTP/1.1 200 OK\r\n");
    assert!(matches!(probe(port).check(), Err(ProbeError::Malformed(_))));

    let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("the listener binds");
    let port = silent
        .local_addr()
        .expect("the listener has an address")
        .port();
    assert!(matches!(probe(port).check(), Err(ProbeError::Timeout(1))));

    drop(silent);
    assert!(matches!(
        probe(port).check(),
        Err(ProbeError::Connect { .. })
    ));
}
