//! Black-box command, transport, pagination, safety, and output tests.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests and their helpers fail the test by panicking."
)]

#[path = "../support/mod.rs"]
mod support;

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tempfile::{TempDir, tempdir};

const MANIFEST: &str = r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[[spec.services]]
name = "web"

[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"
"#;

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    drop_connection: bool,
}

impl Reply {
    /// Replies `status` with `body` as JSON, such as an API error.
    fn status(status: &'static str, body: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(body).expect("JSON response"),
            drop_connection: false,
        }
    }

    /// Replies `status` with `data` in the API's `data` envelope.
    fn enveloped(status: &'static str, data: Value) -> Self {
        let envelope = serde_json::Map::from_iter([("data".to_owned(), data)]);
        Self::status(status, &Value::Object(envelope))
    }

    fn json(data: Value) -> Self {
        Self::enveloped("200 OK", data)
    }

    /// Replies with `data` as the whole body, as authentication endpoints do.
    fn bare(data: &Value) -> Self {
        Self::status("200 OK", data)
    }

    fn accepted(data: Value) -> Self {
        Self::enveloped("202 Accepted", data)
    }

    /// Accepts an exec WebSocket; see [`echo_exec`].
    fn upgrade() -> Self {
        Self {
            status: "101 Switching Protocols",
            content_type: "",
            body: Vec::new(),
            drop_connection: false,
        }
    }

    /// Streams each of `timelines` as a `timeline` server-sent event, then
    /// ends the stream, as the daemon's timeline stream does when it stops.
    fn timelines(timelines: &[Value]) -> Self {
        let mut body = Vec::new();
        for timeline in timelines {
            write!(body, "event: timeline\ndata: {timeline}\n\n").expect("writes to a Vec");
        }
        Self {
            status: "200 OK",
            content_type: "text/event-stream",
            body,
            drop_connection: false,
        }
    }

    /// Streams a timeline of `operation` alone; see [`timeline`].
    fn timeline(operation: &Value) -> Self {
        Self::timelines(&[timeline(operation, &json!([]))])
    }

    fn dropped() -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: Vec::new(),
            drop_connection: true,
        }
    }
}

enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

struct TestServer {
    endpoint: Endpoint,
    records: Arc<Mutex<Vec<Request>>>,
    _directory: TempDir,
    join: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    /// Asks the server to stop accepting, then joins it and returns the
    /// requests it served. For a server that may never reach its request
    /// count, such as one serving `usize::MAX`, once the client exited.
    fn stop(self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        self.finish()
    }

    /// Joins the server without asking it to stop, so it first serves all
    /// its expected requests or waits `ACCEPT_TIMEOUT` for the next one, and
    /// returns the requests it served.
    fn finish(mut self) -> Vec<Request> {
        self.join
            .take()
            .expect("server join handle")
            .join()
            .expect("server thread");
        self.records.lock().clone()
    }

    /// A `piquelctl` command connected to this server.
    fn command(&self) -> Command {
        let mut command = support::command();
        match &self.endpoint {
            Endpoint::Tcp(url) => command.args(["--url", url]),
            Endpoint::Unix(path) => command.args(["--socket", path.to_str().expect("socket path")]),
        };
        command
    }
}

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds how long a connected client can stall a read, so a stalled request
/// fails the test instead of blocking `finish()`.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves `expected_requests` connections, each with one request answered by
/// `handler`, over a Unix socket or TCP. Gives up once no client connected
/// for `ACCEPT_TIMEOUT`, so a request-count mismatch fails fast instead of
/// hanging forever. The listener closes after the last expected request, so
/// a test that asserts a request is never sent serves `usize::MAX` and
/// [`TestServer::stop`]s once the client exited.
fn start_server<F>(unix: bool, expected_requests: usize, handler: F) -> TestServer
where
    F: FnMut(Request) -> Reply + Send + 'static,
{
    let directory = tempdir().expect("temporary server directory");
    let records = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Mutex::new(handler);
    let serve = {
        let (records, stop) = (Arc::clone(&records), Arc::clone(&stop));
        move |mut accept: Accept| {
            let mut deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
            let mut served = 0;
            while served < expected_requests && !stop.load(Ordering::Relaxed) {
                match accept() {
                    Ok(stream) => {
                        served += 1;
                        deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
                        serve_stream(stream, &records, &handler);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        }
    };
    // BSD sockets inherit the listener's nonblocking mode, so each accepted
    // stream is made blocking again, with a read timeout.
    let (endpoint, accept): (_, Accept) = if unix {
        let path = directory.path().join("piqueld.sock");
        let listener = UnixListener::bind(&path).expect("Unix listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking accept applies");
        let accept = move || {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false).expect("blocking request I/O");
            stream
                .set_read_timeout(Some(READ_TIMEOUT))
                .expect("read timeout applies");
            Ok(Box::new(stream) as Box<dyn Stream>)
        };
        (Endpoint::Unix(path), Box::new(accept))
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").expect("TCP listener");
        let address = listener.local_addr().expect("TCP address");
        listener
            .set_nonblocking(true)
            .expect("non-blocking accept applies");
        let accept = move || {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false).expect("blocking request I/O");
            stream
                .set_read_timeout(Some(READ_TIMEOUT))
                .expect("read timeout applies");
            Ok(Box::new(stream) as Box<dyn Stream>)
        };
        (
            Endpoint::Tcp(format!("http://{address}/")),
            Box::new(accept),
        )
    };
    TestServer {
        endpoint,
        records,
        _directory: directory,
        join: Some(thread::spawn(move || serve(accept))),
        stop,
    }
}

/// A connection the test server reads a request from and answers.
trait Stream: Read + Write {}
impl<S: Read + Write> Stream for S {}

/// Accepts the next connection of a nonblocking listener.
type Accept = Box<dyn FnMut() -> std::io::Result<Box<dyn Stream>> + Send>;

fn serve_stream(
    mut stream: Box<dyn Stream>,
    records: &Mutex<Vec<Request>>,
    handler: &Mutex<impl FnMut(Request) -> Reply>,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let websocket_key = request.headers.get("sec-websocket-key").cloned();
    records.lock().push(request.clone());
    let reply = handler.lock()(request);
    if reply.drop_connection {
        return;
    }
    if reply.status == Reply::upgrade().status {
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
            websocket_key.expect("WebSocket key").as_bytes(),
        );
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        echo_exec(stream);
        return;
    }
    let header = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    if let Err(error) = stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(&reply.body))
    {
        // Timeout and interrupt tests deliberately close the client connection.
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ),
            "HTTP response: {error}"
        );
    }
}

/// Checks the exec request for `cat -`, echoes standard input until it
/// closes, then reports stderr and exit code 3. A client that disconnects
/// first gets nothing.
fn echo_exec<S: Read + Write>(stream: S) {
    use piqueld_client::exec::{ExecFrame, ExecInput, ExecOutput};
    use tokio_tungstenite::tungstenite::{Message, WebSocket, protocol::Role};
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let Ok(Message::Text(request)) = socket.read() else {
        panic!("exec request")
    };
    assert_eq!(
        serde_json::from_str::<Value>(request.as_str()).expect("exec request"),
        json!({"service": "web", "command": ["cat", "-"], "stdin": true, "tty": null})
    );
    let mut stdin = Vec::new();
    loop {
        match socket.read() {
            Ok(Message::Binary(data)) => match ExecInput::decode(&data).expect("exec input") {
                ExecInput::Stdin(data) => stdin.extend(data),
                ExecInput::CloseStdin => break,
                frame @ ExecInput::Resize(_) => panic!("unexpected exec input {frame:?}"),
            },
            Ok(message) => panic!("unexpected exec message {message:?}"),
            Err(_) => return,
        }
    }
    for frame in [
        ExecOutput::Stdout(stdin),
        ExecOutput::Stderr(b"closed".to_vec()),
        ExecOutput::Exit(3),
    ] {
        socket
            .send(Message::binary(frame.encode()))
            .expect("exec output");
    }
    let _ = socket.close(None);
}

fn read_request<S: Read>(stream: &mut S) -> Option<Request> {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("HTTP request");
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }

    let body_start = header_end;
    let header_text = String::from_utf8_lossy(&bytes[..body_start - 4]);
    let mut lines = header_text.lines();
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("HTTP method").to_owned();
    let path = normalize_target(request_parts.next().expect("HTTP path"));
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < body_start + content_length {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("HTTP request body");
        assert_ne!(read, 0, "request ended before Content-Length");
        bytes.extend_from_slice(&buffer[..read]);
    }

    Some(Request {
        method,
        path,
        headers,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn normalize_target(target: &str) -> String {
    let Some(authority_and_path) = target.split_once("://").map(|(_, value)| value) else {
        return target.to_owned();
    };
    authority_and_path.find('/').map_or_else(
        || "/".to_owned(),
        |index| authority_and_path[index..].to_owned(),
    )
}

fn run(server: &TestServer, arguments: &[&str]) -> Output {
    run_with_timeout(server, arguments, "2s")
}

fn run_with_timeout(server: &TestServer, arguments: &[&str], timeout: &str) -> Output {
    run_with_format(server, arguments, timeout, true)
}

fn run_human(server: &TestServer, arguments: &[&str]) -> Output {
    run_with_format(server, arguments, "2s", false)
}

fn run_with_format(server: &TestServer, arguments: &[&str], timeout: &str, json: bool) -> Output {
    let mut command = server.command();
    if json {
        command.arg("--json");
    }
    command
        .args(["--timeout", timeout])
        .args(arguments)
        .output()
        .expect("piquelctl process")
}

fn write_manifest(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("application.toml");
    fs::write(&path, MANIFEST).expect("manifest");
    path
}

fn app_view(id: &str, name: &str) -> Value {
    json!({
        "application": {
            "id": id,
            "api_version": "piqueld.dev/v1alpha1",
            "kind": "Application",
            "metadata": {"name": name},
            "spec": {
                "services": [{
                    "name": "web",
                    "source": {"type": "image", "image": "ghcr.io/example/notes:1.4.0"},
                    "replicas": 2,
                    "environment": {},
                    "command": [],
                    "arguments": [],
                    "mounts": [],
                    "healthcheck": null,
                    "resources": null
                }],
                "volumes": [{"name": "data"}]
            }
        },
        "generation": 1, "spec_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "delete_intent": false,
        "created_at_ms": 1,
        "updated_at_ms": 1,
        "environments": [environment(id, id, "production")]
    })
}

/// An environment of application `application`. The fixtures' `production`
/// environments share their application's ID, for brevity.
fn environment(application: &str, id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "application_id": application,
        "name": name,
        "source": {"type": "saved"},
        "resolved_generation": 1,
        "delete_intent": false,
        "created_at_ms": 1,
        "updated_at_ms": 1
    })
}

fn app_summary(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "generation": 1,
        "delete_intent": false,
        "created_at_ms": 1,
        "updated_at_ms": 1,
        "environments": [environment(id, id, "production")]
    })
}

fn page(items: Vec<Value>, next_cursor: Option<&str>) -> Value {
    let items = items.into_iter().collect::<Vec<_>>();
    json!({"items": items, "next_cursor": next_cursor})
}

fn status(id: &str, state: &str) -> Value {
    json!({
        "environment_id": id,
        "state": state,
        "message": null,
        "updated_at_ms": 1
    })
}

fn plan(id: &str) -> Value {
    json!({
        "application_id": id,
        "generation": u64::from(!id.starts_with("preview-")),
        "identical": false, "operation": null, "changes": [],
        "plan": {
            "actions": [],
            "diagnostics": []
        },
        "rollouts": []
    })
}

fn accepted(id: &str) -> Value {
    json!({
        "generation": 1, "operation_id": "operation-01",
        "environment_id": id
    })
}

/// A saved configuration without a deployment, as returned by edits.
fn saved() -> Value {
    json!({"application_id": "app-notes-01", "generation": 2, "operation_id": null})
}

/// An accepted application deletion with one environment.
fn deleted(id: &str) -> Value {
    json!({"application_id": id, "generation": 2, "operations": [accepted(id)]})
}

fn operation(state: &str) -> Value {
    let failed = state == "failed";
    json!({
        "id": "operation-01",
        "environment_id": "app-notes-01",
        "kind": "apply", "generation": 1, "attempt": 1,
        "state": state,
        "error_code": if failed { json!("runtime_failed") } else { Value::Null },
        "error_message": if failed { json!("runtime reconciliation failed") } else { Value::Null },
        "created_at_ms": 1,
        "updated_at_ms": 2,
        "started_at_ms": 1,
        "finished_at_ms": if state == "succeeded" { json!(2) } else { Value::Null }
    })
}

/// The timeline of `operation`, with `steps`.
fn timeline(operation: &Value, steps: &Value) -> Value {
    json!({"operation": operation, "steps": steps, "cursor": 1})
}

/// The path of `operation-01`'s timeline stream.
const TIMELINE_STREAM: &str = "/api/v1/operations/operation-01/timeline/stream";

fn assert_json_success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty(), "JSON stdout must not be empty");
    serde_json::from_slice(&output.stdout).expect("clean JSON stdout")
}

/// `notes`, with its `production` environment and one preview,
/// `env-preview-01`.
fn notes_with_a_preview() -> Value {
    let mut view = app_view("app-notes-01", "notes");
    view["previews"] = json!([environment(
        "app-notes-01",
        "env-preview-01",
        "notes-feat-login-0a1b2c"
    )]);
    view
}

/// Runs `arguments` against a server that serves `view` as `app-notes-01`,
/// asserting they save only `expected` with one `method` request to the
/// application's `path`, conditioned on the inspected revision.
fn assert_saves(
    view: Value,
    arguments: &[&str],
    method: &'static str,
    path: &str,
    expected: &Value,
) -> Output {
    let path =
        format!("/api/v1/applications/app-notes-01/{path}?deploy=false&expected_generation=1");
    let expected = expected.clone();
    let server = start_server(false, 2, move |request| {
        if request.method == "GET" {
            assert_eq!(request.path, "/api/v1/applications/app-notes-01");
            return Reply::json(view.clone());
        }
        assert_eq!(request.method, method);
        assert_eq!(request.path, path);
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            expected
        );
        assert!(request.headers.contains_key("idempotency-key"));
        Reply::json(saved())
    });
    let output = run(&server, arguments);
    assert_eq!(assert_json_success(&output)["generation"], 2);
    assert_eq!(server.finish().len(), 2);
    output
}

/// An application with `production` and `staging` environments.
fn two_environment_view() -> Value {
    let mut view = app_view("app-notes-01", "notes");
    view["environments"]
        .as_array_mut()
        .expect("environments")
        .push(environment("app-notes-01", "env-staging-01", "staging"));
    view
}

/// Serves `view` as `app-notes-01` and answers one `method` request to
/// `path`, which must carry the JSON `body`, with `reply`.
fn request_server(
    view: Value,
    method: &'static str,
    path: &'static str,
    body: Value,
    reply: Reply,
) -> TestServer {
    start_server(false, 2, move |request| {
        if request.path == "/api/v1/applications/app-notes-01" {
            return Reply::json(view.clone());
        }
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            (method, path)
        );
        let sent: Value = serde_json::from_slice(&request.body).expect("JSON body");
        assert_eq!(sent, body);
        reply.clone()
    })
}

/// The environment's detail after `latest` was accepted, with its `web`
/// service healthy or not and one named URL in `url_state`.
fn detail(latest: &str, healthy: bool, url_state: &str) -> Value {
    let mut latest_operation = operation("succeeded");
    latest_operation["id"] = json!(latest);
    let pending = if url_state == "ready" {
        json!([])
    } else {
        json!([{"condition": "https", "message": "Public HTTPS is not verified yet"}])
    };
    json!({
        "environment": environment("app-notes-01", "app-notes-01", "production"),
        "application": app_view("app-notes-01", "notes"),
        "manifest": null,
        "status": status("app-notes-01", "ready"),
        "observed": {
            "services": [{
                "name": "web", "image": null, "desired_replicas": 2, "observed_replicas": 2,
                "healthy_replicas": if healthy { 2 } else { 1 },
                "convergence": if healthy { "converged" } else { "degraded" },
                "diagnostics": []
            }],
            "network_count": 1, "volume_count": 0
        },
        "latest_operation": latest_operation,
        "diagnostics": [],
        "urls": [{
            "name": "web", "url": "https://notes.example.com", "visibility": "public",
            "service": "web", "port": 3000, "state": url_state, "pending": pending
        }]
    })
}

mod accounts;
mod applications;
mod builds;
mod editing;
mod environments;
mod exec;
mod notifications;
mod operations;
mod output;
mod secrets;
mod system;
