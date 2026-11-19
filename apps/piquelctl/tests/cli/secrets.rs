//! Stored secrets: values read from stdin, access, and key recovery.

use super::*;

#[test]
fn ctrl_c_ends_a_pending_secret_read_from_stdin() {
    let (listed, secrets_listed) = mpsc::channel();
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/applications/app-notes-01/secrets" => {
            listed.send(()).expect("test is waiting");
            Reply::json(json!([]))
        }
        path => panic!("unexpected path {path}"),
    });
    let mut child = server
        .command()
        .args(["app", "secret", "app-notes-01"])
        .args(["set", "database-password", "--stdin", "--yes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("piquelctl child");
    // Keep stdin open without data, so the secret read stays pending.
    let stdin = child.stdin.take().expect("child stdin");
    secrets_listed
        .recv_timeout(Duration::from_secs(10))
        .expect("secret metadata requested");
    thread::sleep(Duration::from_millis(200));
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().expect("child status").is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "Ctrl-C did not end the stdin read"
        );
        thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    let output = child.wait_with_output().expect("interrupted child");
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(output.stdout, b"");
    assert_eq!(server.finish().len(), 2, "no secret value was sent");
}

/// `secret set` that changes the access sends the access it read, so the
/// daemon refuses the upload if someone changed it meanwhile.
#[test]
fn secret_set_guards_the_access_it_replaces() {
    let secret = json!({
        "name": "database-password", "generation": 1, "updated_at_ms": 1,
        "access": {"environments": "all", "previews": false}
    });
    let stored = secret.clone();
    let server = start_server(false, 3, move |request| {
        match request.path.split('?').next().unwrap() {
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/applications/app-notes-01/secrets" => Reply::json(json!([secret.clone()])),
            "/api/v1/applications/app-notes-01/secrets/database-password" => {
                Reply::json(stored.clone())
            }
            path => panic!("unexpected path {path}"),
        }
    });
    let mut child = server
        .command()
        .args(["--json", "app", "secret", "app-notes-01"])
        .args(["set", "database-password", "--previews", "--stdin", "--yes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("piquelctl child");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"value")
        .expect("value is written");
    assert_json_success(&child.wait_with_output().expect("piquelctl output"));
    let requests = server.finish();
    assert_eq!(
        requests[2].path,
        "/api/v1/applications/app-notes-01/secrets/database-password\
         ?environments=all&expected_environments=all&expected_previews=false&previews=true"
    );
}

/// A value piped to `--stdin` loses one trailing newline, like `echo` adds,
/// and what remains must still fit: input is never cut short to fit.
#[test]
fn secret_values_from_stdin_drop_one_trailing_newline() {
    const MAX: usize = 512_000;
    let longest = vec![b'a'; MAX];
    let cases: [(Vec<u8>, Option<&[u8]>); 3] = [
        (b"line\nvalue\r\n\n".to_vec(), Some(b"line\nvalue\r\n")),
        ([&longest[..], b"\r\n"].concat(), Some(&longest)),
        ([&longest[..], b"\nTAIL"].concat(), None),
    ];
    for (input, stored) in cases {
        let refusal =
            json!({"code": "conflict", "message": "stop here", "request_id": "request-01"});
        let expected = 2 + usize::from(stored.is_some());
        let server = start_server(false, expected, move |request| {
            match request.path.split('?').next().unwrap() {
                "/api/v1/applications/app-notes-01" => {
                    Reply::json(app_view("app-notes-01", "notes"))
                }
                "/api/v1/applications/app-notes-01/secrets" => Reply::json(json!([])),
                "/api/v1/applications/app-notes-01/secrets/database-password" => {
                    Reply::status("409 Conflict", &refusal)
                }
                path => panic!("unexpected path {path}"),
            }
        });
        let mut child = server
            .command()
            .args(["app", "secret", "app-notes-01"])
            .args(["set", "database-password", "--stdin", "--yes"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("piquelctl child");
        let mut stdin = child.stdin.take().expect("child stdin");
        // The CLI may refuse before reading everything.
        let _ = stdin.write_all(&input);
        drop(stdin);
        let output = child.wait_with_output().expect("piquelctl output");
        let requests = server.finish();
        if let Some(stored) = stored {
            assert_eq!(requests[2].body, stored);
        } else {
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(requests.len(), 2, "no secret value was sent");
        }
    }
}

/// `app secret access` changes access from the access it listed, sent as
/// `expected`, so a change made meanwhile conflicts (exit 3) instead of
/// being overwritten.
#[test]
fn secret_access_changes_send_the_listed_access_as_expected() {
    let listed = json!({"environments": "all", "previews": false});
    let secret = json!({
        "name": "database-password", "generation": 2, "updated_at_ms": 1, "access": listed
    });
    let refusal = json!({
        "code": "secret_access_conflict",
        "message": "the access changed since it was read",
        "request_id": "request-01"
    });
    let server = start_server(false, 3, move |request| {
        match request.path.split('?').next().unwrap() {
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/applications/app-notes-01/secrets" => Reply::json(json!([secret])),
            "/api/v1/applications/app-notes-01/secrets/database-password/access" => {
                Reply::status("409 Conflict", &refusal)
            }
            path => panic!("unexpected path {path}"),
        }
    });
    let output = run(
        &server,
        &[
            "app",
            "secret",
            "app-notes-01",
            "access",
            "database-password",
            "--previews",
        ],
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let requests = server.finish();
    assert_eq!(requests[2].method, "PUT");
    let body: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(
        body,
        json!({"environments": "all", "previews": true, "expected": listed})
    );
}

#[test]
fn secret_key_recovery_requires_confirmation() {
    let server = start_server(false, 1, |request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/system/secrets/recover-key");
        Reply::json(json!({
            "affected_environments": 2,
            "affected_applications": 1,
            "affected_secrets": 3,
            "discarded_versions": 4,
        }))
    });
    let rejected = run(&server, &["--noninteractive", "secrets", "recover-key"]);
    assert!(!rejected.status.success());
    let recovery = run_human(&server, &["secrets", "recover-key", "--yes"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(
        String::from_utf8_lossy(&recovery.stdout)
            .contains("4 values discarded across 3 secrets in 2 environments and 1 applications")
    );
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("ALL applications and environments")
    );
    assert_eq!(
        server.finish().len(),
        1,
        "unconfirmed recovery must not reach the server"
    );
}
