//! Application listing, inspection, planning, applying, renaming and deletion.

use super::*;

/// A saved configuration with an accepted deployment, as returned by apply.
fn saved_deployment(id: &str) -> Value {
    json!({
        "generation": 1, "operation_id": "operation-01",
        "application_id": id
    })
}

/// Application `id` named `name` as the listing shows it, its environment's
/// status `state`.
fn listed(id: &str, name: &str, state: &str) -> Value {
    let mut summary = app_summary(id, name);
    summary["environments"][0]["status"] = status(id, state);
    summary
}

/// `app list` reads every page, and each environment's status from it.
#[test]
fn list_paginates_and_includes_reconciliation_status() {
    // Each page answers only its own cursor.
    let server = start_server(true, 2, move |request| match request.path.as_str() {
        "/api/v1/applications?limit=100" => Reply::json(page(
            vec![listed("app-first-01", "first", "ready")],
            Some("v1:app-first-01"),
        )),
        "/api/v1/applications?cursor=v1%3Aapp-first-01&limit=100" => Reply::json(page(
            vec![listed("app-notes-01", "notes", "degraded")],
            None,
        )),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&server, &["app", "list"]);
    let value = assert_json_success(&output);
    assert_eq!(value["items"].as_array().expect("items").len(), 2);
    assert_eq!(
        value["items"][1]["environments"][0]["status"]["state"],
        "degraded"
    );
    assert_eq!(output.stderr, b"");
    let _ = server.finish();
}

#[test]
fn repeated_pagination_cursor_is_rejected() {
    let mut page_number = 0;
    let server = start_server(false, 2, move |request| {
        page_number += 1;
        // The second page is requested with the first page's cursor, which
        // it returns again.
        let (path, items) = if page_number == 1 {
            (
                "/api/v1/applications?limit=100",
                vec![app_summary("app-first-01", "first")],
            )
        } else {
            (
                "/api/v1/applications?cursor=v1%3Aapp-first-01&limit=100",
                Vec::new(),
            )
        };
        assert_eq!(request.path, path);
        Reply::json(page(items, Some("v1:app-first-01")))
    });

    let output = run(&server, &["app", "list"]);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("repeated pagination cursor"));
    let _ = server.finish();
}

#[test]
fn show_resolves_a_name_through_the_daemon_and_an_id_directly() {
    let first_server = start_server(false, 3, move |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => {
            Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
        }
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/environments/app-notes-01/status" => Reply::json(status("app-notes-01", "ready")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&first_server, &["app", "show", "notes"]);
    let value = assert_json_success(&output);
    assert_eq!(
        value["application"]["application"]["metadata"]["name"],
        "notes"
    );
    let _ = first_server.finish();

    let second_server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/environments/app-notes-01/status" => Reply::json(status("app-notes-01", "ready")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&second_server, &["app", "show", "app-notes-01"]);
    let value = assert_json_success(&output);
    assert_eq!(value["environments"][0]["status"]["state"], "ready");
    let _ = second_server.finish();
}

#[test]
fn apply_deploy_retries_a_dropped_request_with_the_same_identity() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let mut attempts = 0;
    // Over the Unix socket, the default transport; other retries run over TCP.
    let server = start_server(true, 3, move |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => Reply::json(page(Vec::new(), None)),
        "/api/v1/applications/apply?deploy=true" => {
            attempts += 1;
            if attempts == 1 {
                Reply::dropped()
            } else {
                Reply::accepted(saved_deployment("app-notes-01"))
            }
        }
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "--request-id",
            "agent-run-7",
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().expect("manifest path"),
            "--yes",
            "--no-wait",
        ],
    );
    let value = assert_json_success(&output);
    assert_eq!(value["application_id"], "app-notes-01");
    assert!(output.stderr.is_empty(), "preview is a separate command");
    let records = server.finish();
    let [first, second] = &records[1..] else {
        panic!("one retry: {records:?}");
    };
    for mutation in [first, second] {
        assert_eq!(mutation.body, MANIFEST.as_bytes());
        assert_eq!(mutation.headers["content-type"], "application/toml");
        assert_eq!(mutation.headers["x-expected-generation"], "0");
    }
    let key = first
        .headers
        .get("idempotency-key")
        .expect("command has a replay identity");
    // Sent as is, so rerunning the command replays it.
    assert_eq!(key, "agent-run-7");
    assert_eq!(Some(key), second.headers.get("idempotency-key"));
}

#[test]
fn noninteractive_apply_stops_after_inspection() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(false, usize::MAX, move |request| {
        match request.path.as_str() {
            "/api/v1/applications?name=notes" => Reply::json(page(Vec::new(), None)),
            path => panic!("mutation must not be sent; got {path}"),
        }
    });
    let output = run(
        &server,
        &[
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().expect("manifest path"),
        ],
    );
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirmation"));
    assert_eq!(server.stop().len(), 1);
}

#[test]
fn human_plan_keeps_actions_and_diagnostics_when_configuration_matches_the_deployment() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    for identical in [false, true] {
        let server = start_server(true, 1, move |_| {
            let mut preview = plan("app-notes-01");
            preview["identical"] = json!(identical);
            preview["operation"] = operation("succeeded");
            if !identical {
                preview["changes"] = json!([{
                    "field": "services.web.replicas",
                    "before": null,
                    "after": "1"
                }]);
            }
            preview["plan"]["actions"] = json!([{
                "kind": {"action": "resolve_image", "service": "web", "reference": "nginx:alpine"},
                "reason": {"reason": "resolution_required"}
            }]);
            preview["plan"]["diagnostics"] = json!([{
                "code": "runtime_unavailable", "severity": "warning", "resource": "web",
                "message": "runtime inspection is unavailable", "blocking": false
            }]);
            Reply::json(preview)
        });

        let output = run_human(
            &server,
            &[
                "app",
                "plan",
                "--file",
                manifest.to_str().expect("manifest path"),
            ],
        );
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        if identical {
            assert!(stdout.contains("Configuration matches the latest deployment snapshot."));
        } else {
            assert!(stdout.contains("Application: app-notes-01\n\nChanges:"));
            assert!(stdout.contains("+ services.web.replicas"));
        }
        assert!(stdout.contains("Actions: 1 total"));
        assert!(stdout.contains("1. Resolve image web\n      no risk · resolution required"));
        assert!(stdout.contains("runtime inspection is unavailable"));
        let _ = server.finish();
    }
}

/// `app plan` asks only for a plan, and lists how saving the manifest would
/// change the environments it declares, marking each change.
#[test]
fn human_plan_lists_environment_changes() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(true, 1, |request| {
        assert_eq!(request.path, "/api/v1/applications/plan");
        let mut preview = plan("app-notes-01");
        preview["environments"] = json!([
            {"change": "create", "environment": "staging", "source": {"type": "branch", "branch": "main"}, "sync": true},
            {"change": "source", "environment": "production",
             "from": {"type": "branch", "branch": "main"}, "to": {"type": "promoted", "from": "staging"}},
            {"change": "undeclared", "environment": "qa"},
        ]);
        Reply::json(preview)
    });
    let output = run_human(
        &server,
        &[
            "app",
            "plan",
            "--file",
            manifest.to_str().expect("manifest path"),
        ],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        "Environments:",
        "+ staging",
        "created, deploys from branch main, follows pushes",
        "~ production",
        "deploys from releases promoted from staging instead of branch main",
        "! qa",
        "no longer declared; kept until deleted with `piquelctl env delete`",
    ] {
        assert!(stdout.contains(line), "{line:?} missing from {stdout}");
    }
    let _ = server.finish();
}

#[test]
fn blocked_plan_emits_json_then_fails_and_quiet_keeps_the_reason() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    for json in [false, true] {
        let server = start_server(false, 1, |_| {
            let mut preview = plan("preview-00000001");
            preview["plan"]["diagnostics"] = json!([{
                "code": "ownership_conflict", "severity": "error", "resource": "network", "message": "owned by another application", "blocking": true
            }]);
            Reply::json(preview)
        });
        let output = run_with_format(
            &server,
            &[
                "--quiet",
                "app",
                "plan",
                "--file",
                manifest.to_str().unwrap(),
            ],
            "2s",
            json,
        );
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if json {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["plan"]["diagnostics"][0]["blocking"], true);
        } else {
            assert_eq!(output.stdout, b"");
        }
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("ownership_conflict [network]: owned by another application")
        );
        server.finish();
    }
}

/// A failed deployment exits 5, reporting the failure with bounded context
/// instead of the raw operation.
#[test]
fn apply_reports_a_failed_operation_with_exit_5_and_bounded_context() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(true, 3, move |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => Reply::json(page(Vec::new(), None)),
        "/api/v1/applications/apply?deploy=true" => {
            Reply::accepted(saved_deployment("app-notes-01"))
        }
        TIMELINE_STREAM => {
            let mut value = operation("failed");
            value["phase"] = json!("ensure_network");
            value["resource"] = json!("app-notes-network");
            Reply::timeline(&value)
        }
        path => panic!("unexpected path {path}"),
    });

    let output = run_human(
        &server,
        &[
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().expect("manifest path"),
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Context:"));
    assert!(stderr.contains("Resource: app-notes-network"));
    assert!(stderr.contains("Code: runtime_failed"));
    assert!(stderr.contains("Message: runtime reconciliation failed"));
    assert!(stderr.contains("Hint: retry with `piquelctl env reconcile <APP> app-notes-01`"));
    assert!(!stderr.contains("created_at_ms"));
    let _ = server.finish();
}

/// Missing, oversized, and non-regular manifests fail as input errors
/// before the daemon is contacted, without opening a FIFO.
#[test]
fn unreadable_manifests_fail_before_network_use() {
    let directory = tempdir().expect("manifest directory");
    let oversized = directory.path().join("oversized.toml");
    fs::write(&oversized, vec![b'x'; 4 * 1024 * 1024 + 1]).expect("oversized manifest");
    let fifo = directory.path().join("manifest.fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo")
            .success()
    );
    let server = start_server(false, 0, |_| panic!("manifest input must fail locally"));
    for (path, expected) in [
        (directory.path().join("missing.toml"), "missing.toml"),
        (oversized, "exceeds"),
        (fifo, "not a regular file"),
    ] {
        let output = run(
            &server,
            &[
                "app",
                "plan",
                "--file",
                path.to_str().expect("manifest path"),
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
    }
    let _ = server.finish();
}

#[test]
fn validate_checks_manifests_without_a_daemon_or_profiles() {
    let directory = tempdir().expect("manifest directory");
    let valid = write_manifest(&directory);
    let invalid = directory.path().join("invalid.toml");
    fs::write(
        &invalid,
        MANIFEST
            .replace("v1alpha1", "v2")
            .replace("name = \"web\"", "name = \"Web!\""),
    )
    .expect("invalid manifest");
    let validate = |path: &PathBuf| {
        support::command()
            // Neither the daemon nor the profiles file is consulted.
            .env(
                "PIQUELD_PROFILES_FILE",
                directory.path().join("absent.toml"),
            )
            .args(["--socket", "/nonexistent/piqueld.sock", "--json"])
            .args(["app", "validate", "--file"])
            .arg(path)
            .output()
            .expect("piquelctl process")
    };

    let value = assert_json_success(&validate(&valid));
    assert_eq!(value, json!({"application": "notes"}));

    let output = validate(&invalid);
    assert_eq!(output.status.code(), Some(2));
    let error = support::json_error(&output);
    assert_eq!(error["code"], "invalid_input");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("failed validation with 2 error(s)")
    );
    let paths: Vec<_> = error["details"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["api_version", "spec.services[0].name"]);
}

#[test]
fn delete_reports_named_volume_retention_and_operation_completion() {
    let mut deleting = false;
    let server = start_server(false, 4, move |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => {
            Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
        }
        "/api/v1/applications/app-notes-01" if !deleting => {
            Reply::json(app_view("app-notes-01", "notes"))
        }
        "/api/v1/applications/app-notes-01?expected_generation=1" => {
            deleting = true;
            Reply::accepted(deleted("app-notes-01"))
        }
        "/api/v1/applications/app-notes-01" => Reply::status(
            "404 Not Found",
            &json!({"code":"not_found","message":"Application deleted"}),
        ),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&server, &["app", "delete", "notes", "--yes"]);
    let value = assert_json_success(&output);
    assert_eq!(value["volumes_retained"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("named volumes are retained"));
    let _ = server.finish();
}

#[test]
fn delete_stops_on_failed_or_cancelled_operation() {
    for state in ["failed", "cancelled"] {
        let server = start_server(false, 5, move |request| match request.path.as_str() {
            "/api/v1/applications?name=notes" => {
                Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
            }
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/applications/app-notes-01?expected_generation=1" => {
                Reply::accepted(deleted("app-notes-01"))
            }
            "/api/v1/operations/operation-01" => Reply::json(operation(state)),
            path => panic!("unexpected path {path}"),
        });
        let output = run(&server, &["app", "delete", "notes", "--yes"]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(if state == "failed" {
                "runtime reconciliation failed"
            } else {
                "cancelled"
            }),
            "{stderr}"
        );
        let _ = server.finish();
    }
}

/// A name the daemon doesn't find, or that it answers with a different
/// application, is an input error.
#[test]
fn names_are_looked_up_by_the_daemon_and_checked() {
    // A daemon that predates the name filter answers with other applications.
    for (applications, code, expected) in [
        (Vec::new(), 2, "was not found"),
        (vec![app_summary("app-blog-01", "blog")], 2, "was not found"),
    ] {
        let server = start_server(false, 1, move |request| match request.path.as_str() {
            "/api/v1/applications?name=notes" => Reply::json(page(applications.clone(), None)),
            path => panic!("unexpected path {path}"),
        });
        let output = run(&server, &["app", "show", "notes"]);
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(output.stdout, b"");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        let _ = server.finish();
    }
}

#[test]
fn apply_defaults_to_saving_without_preview_or_operation_polling() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    let server = start_server(false, 2, |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => {
            Reply::json(json!({"items":[app_summary("app-notes-01","notes")],"next_cursor":null}))
        }
        "/api/v1/applications/apply?deploy=false" => {
            assert_eq!(
                request
                    .headers
                    .get("x-expected-generation")
                    .map(String::as_str),
                Some("1")
            );
            Reply::json(saved())
        }
        _ => panic!("unexpected request {}", request.path),
    });
    let output = run(
        &server,
        &[
            "app",
            "apply",
            "--file",
            manifest.to_str().unwrap(),
            "--yes",
        ],
    );
    let saved = assert_json_success(&output);
    assert_eq!(saved["generation"], 2);
    assert!(saved["operation_id"].is_null());
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn apply_protects_the_inspected_identity_and_revision_with_confirmation_skipped() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    let server = start_server(false, usize::MAX, move |request| {
        if request.path == "/api/v1/applications?name=notes" {
            let mut app = app_summary("app-notes-01", "notes");
            app["generation"] = json!(7);
            return Reply::json(json!({"items":[app],"next_cursor":null}));
        }
        assert_eq!(
            request
                .headers
                .get("x-expected-generation")
                .map(String::as_str),
            Some("7")
        );
        assert_eq!(
            request
                .headers
                .get("x-expected-application-id")
                .map(String::as_str),
            Some("app-notes-01")
        );
        Reply::status(
            "409 Conflict",
            &json!({"code":"generation_conflict","message":"Application changed since inspection; run the command again"}),
        )
    });
    let output = run(
        &server,
        &[
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(3));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("run the command again"));
    assert!(
        !error.contains("Endpoint:"),
        "application conflicts are not connection failures: {error}"
    );
    assert_eq!(
        server.stop().len(),
        2,
        "generation conflicts must not retry"
    );
}

#[test]
fn rename_uses_the_inspected_revision_and_preserves_identity() {
    let output = assert_saves(
        app_view("app-notes-01", "notes"),
        &["app", "rename", "app-notes-01", "renamed", "--yes"],
        "PUT",
        "name",
        &json!({"value":"renamed"}),
    );
    assert_eq!(
        assert_json_success(&output)["application_id"],
        "app-notes-01"
    );
    assert_eq!(output.stderr, b"");
}

#[test]
fn force_does_not_skip_confirmation_and_explicit_force_is_sent_to_the_endpoint() {
    for yes in [false, true] {
        let directory = tempdir().unwrap();
        let manifest = write_manifest(&directory);
        let mut mutations = 0;
        let server = start_server(false, if yes { 3 } else { 1 }, move |request| {
            if request.path == "/api/v1/applications?name=notes" {
                return Reply::json(page(Vec::new(), None));
            }
            assert_eq!(
                request.path,
                "/api/v1/applications/apply?force=true&deploy=true"
            );
            assert!(!request.headers.contains_key("x-expected-generation"));
            assert!(!request.headers.contains_key("x-expected-application-id"));
            mutations += 1;
            if mutations == 1 {
                Reply::dropped()
            } else {
                Reply::accepted(saved_deployment("app-notes-01"))
            }
        });
        let mut args = vec![
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().unwrap(),
            "--force",
            "--no-wait",
        ];
        if yes {
            args.push("--yes");
        }
        let output = run(&server, &args);
        if yes {
            assert_json_success(&output);
        } else {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
        }
        let requests = server.finish();
        if yes {
            let key = requests[1].headers.get("idempotency-key");
            assert!(key.is_some(), "the forced mutation has an idempotency key");
            assert_eq!(key, requests[2].headers.get("idempotency-key"));
        }
    }
}

#[test]
fn apply_returns_superseded_success_without_following_the_replacement() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    let server = start_server(false, 3, |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => Reply::json(page(Vec::new(), None)),
        "/api/v1/applications/apply?deploy=true" => {
            Reply::accepted(saved_deployment("app-notes-01"))
        }
        TIMELINE_STREAM => Reply::timeline(&operation("superseded")),
        _ => panic!("unexpected request {}", request.path),
    });
    let output = run(
        &server,
        &[
            "app",
            "apply",
            "--deploy",
            "--file",
            manifest.to_str().unwrap(),
            "--yes",
        ],
    );
    let result = assert_json_success(&output);
    assert_eq!(result["outcome"], "superseded");
    assert_eq!(result["operation"]["state"], "superseded");
    assert_eq!(server.finish().len(), 3);
}
