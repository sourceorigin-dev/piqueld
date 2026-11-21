//! Output modes, transport retries and errors shared by every command.

use super::*;

/// A status that can't be read leaves the other environments listed, and
/// warns with its context.
#[test]
fn partial_list_failures_preserve_results_and_context_in_all_modes() {
    let mut application = app_view("app-notes-01", "notes");
    application["environments"]
        .as_array_mut()
        .expect("environments")
        .push(environment("app-notes-01", "env-staging-01", "staging"));
    for json in [false, true] {
        for quiet in [false, true] {
            let application = application.clone();
            let server = start_server(false, 3, move |request| match request.path.as_str() {
                "/api/v1/applications/app-notes-01" => Reply::json(application.clone()),
                "/api/v1/environments/app-notes-01/status" => {
                    Reply::json(status("app-notes-01", "ready"))
                }
                "/api/v1/environments/env-staging-01/status" => Reply {
                    content_type: "text/html",
                    body: b"<html>wrong service</html>".to_vec(),
                    ..Reply::json(Value::Null)
                },
                path => panic!("unexpected path {path}"),
            });
            let args = if quiet {
                vec!["--quiet", "app", "show", "app-notes-01"]
            } else {
                vec!["app", "show", "app-notes-01"]
            };
            let output = run_with_format(&server, &args, "2s", json);
            assert!(output.status.success());
            if json {
                let value = assert_json_success(&output);
                assert_eq!(value["environments"][0]["status"]["state"], "ready");
                assert!(value["environments"][1]["status"].is_null());
                assert_eq!(value["environments"][1]["environment"]["name"], "staging");
            } else if quiet {
                assert_eq!(output.stdout, b"");
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("staging (env-staging-01): unavailable")
                );
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if json {
                let warning: Value = serde_json::from_str(stderr.trim()).unwrap();
                let message = warning["warning"]["message"].as_str().unwrap();
                assert!(message.starts_with("notes/staging: status unavailable"));
            } else {
                assert!(stderr.contains("Warning: notes/staging: status unavailable"));
                assert!(stderr.contains("Endpoint:"));
                assert!(stderr.contains("Endpoint source: flag --url"));
                assert!(stderr.contains("Hint:"));
            }
            assert!(!stderr.contains("<html>"));
            server.finish();
        }
    }
}

#[test]
fn read_request_uses_the_full_command_timeout() {
    let server = start_server(false, 1, |_| {
        thread::sleep(Duration::from_millis(650));
        Reply::json(
            json!({"status": "ok", "daemon_version": "test", "api_version": "v1", "instance_id": "test"}),
        )
    });
    let output = run_with_timeout(&server, &["status"], "1s");
    assert_json_success(&output);
    server.finish();
}

#[test]
fn human_output_reports_a_closed_pipe_without_panicking() {
    let server = start_server(false, 1, |_| Reply::json(operation("requested")));
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("output pipe");
    drop(reader);
    let output = server
        .command()
        .args(["operation", "operation-01", "--no-wait"])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .output()
        .expect("CLI reports output failure");
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("could not write output"), "stderr: {error}");
    assert!(!error.contains("panicked"));
    server.finish();
}

#[test]
fn runtime_commands_retry_transport() {
    for action in ["reconcile", "deploy", "stop", "start"] {
        let mut attempts = 0;
        let server = start_server(false, 3, move |request| {
            if request.method == "GET" {
                return Reply::json(app_view("app-notes-01", "notes"));
            }
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.path,
                format!(
                    "/api/v1/environments/app-notes-01/{action}{}",
                    if action == "deploy" {
                        "?expected_generation=1"
                    } else {
                        ""
                    }
                )
            );
            attempts += 1;
            if attempts == 1 {
                Reply::dropped()
            } else {
                Reply::accepted(accepted("app-notes-01"))
            }
        });
        let output = run(
            &server,
            &["app", action, "app-notes-01", "--yes", "--no-wait"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["generation"], 1);
        assert_eq!(server.finish().len(), 3);
    }
}

#[test]
fn quiet_preserves_json_and_errors_but_suppresses_human_success() {
    for json in [false, true] {
        let server = start_server(false, 1, |_| {
            Reply::json(json!({
                "status":"running", "api_version":"v1", "daemon_version":"test", "instance_id":"instance-test"
            }))
        });
        let output = run_with_format(&server, &["--quiet", "status"], "2s", json);
        assert!(output.status.success());
        assert_eq!(output.stderr, b"");
        if json {
            assert_json_success(&output);
        } else {
            assert_eq!(output.stdout, b"");
        }
        server.finish();
    }
    let server = start_server(false, 1, |_| Reply::json(page(Vec::new(), None)));
    let output = run_human(&server, &["--quiet", "builds", "list"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
    server.finish();

    let server = start_server(false, 1, |_| {
        Reply::json(json!({
            "items": [{"offset":64,"timestamp_ms":1000,"stream":"stdout","text":"build output"}],
            "previous_offset": 64,
            "truncated": true,
            "expired": true
        }))
    });
    let output = run_human(&server, &["--quiet", "builds", "logs", "1"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("expired"));
    assert!(stderr.contains("truncated"));
    assert!(!stderr.contains("--before"));
    server.finish();

    let output = support::command()
        .args(["--quiet", "--socket", "/nonexistent/piqueld.sock", "status"])
        .output()
        .expect("CLI");
    assert!(!output.status.success());
    assert_ne!(output.stderr, b"");
}

#[test]
fn unexpected_responses_include_connection_context_without_extra_probes() {
    for (status, body, expected) in [
        ("200 OK", "{}", "missing field `data`"),
        (
            "404 Not Found",
            "<html>not found</html>",
            "HTTP 404 Not Found",
        ),
    ] {
        let server = start_server(false, usize::MAX, move |_| Reply {
            status,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
            drop_connection: false,
        });
        let output = run_human(&server, &["--quiet", "status"]);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("Endpoint: TCP http://127.0.0.1:"), "{error}");
        assert!(error.contains("Endpoint source: flag --url"), "{error}");
        assert!(
            error.contains("Check that the selected endpoint serves the piqueld API"),
            "{error}"
        );
        assert_eq!(server.stop().len(), 1);
    }
}

#[test]
fn json_errors_are_one_envelope_on_stderr() {
    let details = json!({"limit": "per_application", "max": 1, "previews": []});
    let body = json!({
        "code": "preview_limit_reached", "message": "preview limit reached",
        "details": details, "request_id": "request-01"
    });
    let server = start_server(false, 2, move |request| {
        match request.path.split('?').next().unwrap() {
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/applications/app-notes-01/previews" => Reply::status("409 Conflict", &body),
            path => panic!("unexpected path {path}"),
        }
    });
    let output = run(
        &server,
        &["preview", "create", "app-notes-01", "--branch", "feat"],
    );
    server.finish();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        support::json_error(&output),
        json!({
            "code": "preview_limit_reached", "message": "preview limit reached",
            "details": details, "request_id": "request-01"
        })
    );

    // Usage errors take the same shape.
    let output = support::command()
        .args(["--json", "env", "wait"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = support::json_error(&output);
    assert_eq!(error["code"], "usage");
    assert!(error["message"].as_str().unwrap().contains("required"));
}
