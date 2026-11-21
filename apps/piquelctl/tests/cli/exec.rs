//! `app exec` and `preview exec` over the exec WebSocket.

use super::*;

/// Runs `<target> web -i -- cat -` against an echo server, feeding `stdin`.
/// The command must run in environment `runs_in` of `notes`, which has one
/// preview, `env-preview-01`.
fn exec_cat(target: &[&str], runs_in: &str, stdin: Stdio, input: &[u8]) -> (TestServer, Output) {
    let exec = format!("/api/v1/environments/{runs_in}/exec");
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(notes_with_a_preview()),
        path if path == exec => {
            assert_eq!(request.method, "GET");
            assert_eq!(request.headers["upgrade"], "websocket");
            Reply::upgrade()
        }
        path => panic!("unexpected path {path}"),
    });
    let mut child = server
        .command()
        .args(target)
        .args(["web", "-i", "--", "cat", "-"])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("piquelctl process");
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(input).expect("stdin is forwarded");
    }
    let output = child.wait_with_output().expect("piquelctl exits");
    (server, output)
}

#[test]
fn exec_ends_through_the_supervisor_on_sigterm() {
    let (started, session_started) = mpsc::channel();
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/environments/app-notes-01/exec" => Reply::upgrade_signalling(started.clone()),
        path => panic!("unexpected path {path}"),
    });
    // Standard input stays open, so the session never ends on its own.
    let mut child = server
        .command()
        .args(["app", "exec", "app-notes-01", "web", "-i", "--", "cat", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("piquelctl process");
    let stdin = child.stdin.take().expect("child stdin");
    // The supervisor's handlers are installed before any request is sent, and
    // the exec request was read, so SIGTERM arrives during the session.
    session_started
        .recv_timeout(Duration::from_secs(10))
        .expect("exec requested");
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    let output = child.wait_with_output().expect("terminated child");
    drop(stdin);
    let _ = server.finish();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("SIGTERM: command interrupted"),
        "{output:?}"
    );
}

#[test]
fn exec_forwards_stdin_streams_output_and_exits_with_the_command_code() {
    for (target, runs_in) in [
        (&["app", "exec", "app-notes-01"][..], "app-notes-01"),
        (
            &["preview", "exec", "app-notes-01", "notes-feat-login-0a1b2c"],
            "env-preview-01",
        ),
    ] {
        let (server, output) = exec_cat(target, runs_in, Stdio::piped(), b"hello");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"closed");
        let _ = server.finish();
    }
}

#[test]
fn exec_fails_instead_of_closing_stdin_when_it_cannot_be_read() {
    // Reading a directory fails with EISDIR.
    let directory = fs::File::open(env!("CARGO_MANIFEST_DIR")).expect("directory");
    let target = ["app", "exec", "app-notes-01"];
    let (server, output) = exec_cat(&target, "app-notes-01", directory.into(), b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("could not read standard input"), "{stderr}");
    let _ = server.finish();
}

/// `--tty` needs a terminal on standard input, so it fails before the daemon
/// is contacted rather than running the command without one.
#[test]
fn exec_tty_refuses_without_a_terminal() {
    let server = start_server(false, 0, |_| panic!("--tty must fail locally"));
    for tty in ["--tty", "-t"] {
        let output = run(
            &server,
            &["app", "exec", "app-notes-01", "web", tty, "--", "sh"],
        );
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let error = support::json_error(&output);
        assert_eq!(
            error["message"],
            "--tty requires standard input to be a terminal"
        );
    }
    let _ = server.finish();
}
