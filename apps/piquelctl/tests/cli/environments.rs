//! Environment commands: targeting, promotion, rollback, history and waits.

use super::*;

#[test]
fn runtime_app_commands_never_pick_one_of_several_environments() {
    for action in ["deploy", "reconcile", "stop", "start", "logs"] {
        let server = start_server(false, usize::MAX, move |request| {
            match request.path.as_str() {
                "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
                path => panic!("unexpected path {path}"),
            }
        });
        let mut args = vec!["app", action, "app-notes-01"];
        if action != "logs" {
            args.push("--yes");
        }
        let output = run(&server, &args);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("environment required"), "{stderr}");
        assert!(stderr.contains("production, staging"), "{stderr}");
        assert_eq!(server.finish().len(), 1, "no mutation was sent");
    }
}

#[test]
fn env_commands_target_the_named_environment() {
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
        "/api/v1/environments/env-staging-01/deploy?expected_generation=1" => {
            assert_eq!(request.method, "POST");
            Reply::accepted(accepted("env-staging-01"))
        }
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "env",
            "deploy",
            "app-notes-01",
            "staging",
            "--yes",
            "--no-wait",
        ],
    );
    assert_eq!(
        assert_json_success(&output)["environment_id"],
        "env-staging-01"
    );
    let _ = server.finish();

    let staging = Reply::json(environment("app-notes-01", "env-staging-01", "staging"));
    let server = request_server(
        app_view("app-notes-01", "notes"),
        "POST",
        "/api/v1/applications/app-notes-01/environments",
        json!({"name": "staging", "branch": "release", "expected_generation": 1}),
        staging.clone(),
    );
    let output = run(
        &server,
        &[
            "env",
            "create",
            "app-notes-01",
            "staging",
            "--branch",
            "release",
            "--yes",
        ],
    );
    assert_eq!(assert_json_success(&output)["name"], "staging");
    let _ = server.finish();

    let commit = "a".repeat(40);
    let server = request_server(
        two_environment_view(),
        "PUT",
        "/api/v1/environments/env-staging-01/branch",
        json!({"branch": "main", "commit": commit, "expected_generation": 1}),
        staging,
    );
    let output = run(
        &server,
        &[
            "env",
            "branch",
            "app-notes-01",
            "staging",
            "main",
            "--commit",
            &commit,
            "--yes",
        ],
    );
    assert_eq!(assert_json_success(&output)["name"], "staging");
    let _ = server.finish();
}

#[test]
fn deleting_an_application_with_several_environments_names_every_one() {
    let server = start_server(false, usize::MAX, move |request| {
        match request.path.as_str() {
            "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
            path => panic!("unexpected path {path}"),
        }
    });
    let output = run(&server, &["app", "delete", "app-notes-01", "--yes"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--environments production,staging"));
    assert_eq!(server.finish().len(), 1, "no deletion was sent");

    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
        "/api/v1/applications/app-notes-01?environments=staging%2Cproduction&expected_generation=1" =>
        {
            assert_eq!(request.method, "DELETE");
            Reply::accepted(deleted("app-notes-01"))
        }
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "app",
            "delete",
            "app-notes-01",
            "--environments",
            "staging,production",
            "--yes",
            "--no-wait",
        ],
    );
    assert_eq!(assert_json_success(&output)["volumes_retained"], true);
    let _ = server.finish();
}

#[test]
fn stable_ids_select_environments_before_names_that_look_like_them() {
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => {
            let mut view = app_view("app-notes-01", "notes");
            // Name order lists the impostor named like production's ID first.
            view["environments"]
                .as_array_mut()
                .expect("environments")
                .insert(
                    0,
                    environment("app-notes-01", "env-impostor-01", "app-notes-01"),
                );
            Reply::json(view)
        }
        "/api/v1/environments/app-notes-01?expected_generation=1" => {
            assert_eq!(request.method, "DELETE");
            Reply::accepted(accepted("app-notes-01"))
        }
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "env",
            "delete",
            "app-notes-01",
            "app-notes-01",
            "--yes",
            "--no-wait",
        ],
    );
    assert_eq!(
        assert_json_success(&output)["accepted"]["environment_id"],
        "app-notes-01"
    );
    let _ = server.finish();
}

#[test]
fn plan_compares_with_the_named_environment() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(false, 3, move |request| match request.path.as_str() {
        "/api/v1/applications?name=notes" => {
            Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
        }
        "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
        "/api/v1/applications/plan?environment=env-staging-01" => Reply::json(plan("app-notes-01")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "app",
            "plan",
            "--file",
            manifest.to_str().expect("manifest path"),
            "--env",
            "staging",
        ],
    );
    assert_eq!(
        assert_json_success(&output)["application_id"],
        "app-notes-01"
    );
    let _ = server.finish();
}

/// `shop`'s production, promoted from its staging environment.
fn promoted_view() -> Value {
    let mut view = two_environment_view();
    view["environments"][0]["source"] =
        json!({"type": "promoted", "environment": "env-staging-01"});
    view
}

/// A plan of promoting staging's release into production, which lacks its
/// own API key.
fn promotion_plan() -> Value {
    let mut view = plan("app-notes-01");
    let digest = format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64));
    view["release"] = json!({
        "release": {
            "id": "rel-0123456789abcdef",
            "application_id": "app-notes-01",
            "content_hash": format!("sha256:{}", "b".repeat(64)),
            "created_at_ms": 1,
            "release": {
                "template": app_view("app-notes-01", "notes")["application"],
                "sources": {"web": {"type": "image", "requested": "ghcr.io/example/notes:1.4.0", "digest_reference": digest}}
            },
            "fingerprint": {"web": {"source.image": "ghcr.io/example/notes:1.4.0"}},
            "promotions": []
        },
        "origin": {"type": "promotion", "environment": "env-staging-01", "deployment": "operation-01"},
        "new_volumes": ["data"],
        "secrets": [{"problem": "missing", "secret": "production-api-key"}]
    });
    view
}

/// Promotion output names environments, not their IDs; a plan the
/// environment lacks secrets for exits as a conflict, listing them; and an
/// accepted promotion reports the release it pinned and where it came from.
#[test]
fn promotion_commands_name_environments_and_report_the_pinned_release() {
    let server = request_server(
        two_environment_view(),
        "PUT",
        "/api/v1/environments/app-notes-01/source",
        json!({"promote_from": "env-staging-01", "expected_generation": 1}),
        Reply::json(promoted_view()["environments"][0].clone()),
    );
    let output = run_human(
        &server,
        &[
            "env",
            "source",
            "app-notes-01",
            "production",
            "--promote-from",
            "staging",
            "--yes",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("deploys from releases promoted from staging."),
        "{output:?}"
    );
    let _ = server.finish();

    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(promoted_view()),
        "/api/v1/environments/app-notes-01/promote/plan" => Reply::json(promotion_plan()),
        path => panic!("unexpected path {path}"),
    });
    let output = run_human(
        &server,
        &["env", "promote", "app-notes-01", "production", "--plan"],
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("promoted from staging's deployment operation-01"),
        "{stdout}"
    );
    assert!(stdout.contains("New volume"), "{stdout}");
    assert!(stdout.contains("production-api-key"), "{stdout}");
    let _ = server.finish();

    let server = request_server(
        promoted_view(),
        "POST",
        "/api/v1/environments/app-notes-01/promote",
        json!({"deployment": "operation-01", "expected_generation": 1}),
        promotion_accepted(),
    );
    let output = run(
        &server,
        &[
            "env",
            "promote",
            "app-notes-01",
            "production",
            "--deployment",
            "operation-01",
            "--yes",
            "--no-wait",
        ],
    );
    let report = assert_json_success(&output);
    assert_eq!(report["deployment_id"], "operation-02");
    assert_eq!(report["operation_id"], "operation-02");
    assert_eq!(report["release_id"], "rel-0123456789abcdef");
    assert_eq!(report["origin"]["environment"], "env-staging-01");
    let _ = server.finish();
}

/// The accepted promotion of staging's `operation-01` into production.
fn promotion_accepted() -> Reply {
    Reply::accepted(json!({
        "operation_id": "operation-02", "environment_id": "app-notes-01", "generation": 1,
        "release_id": "rel-0123456789abcdef",
        "origin": {"type": "promotion", "environment": "env-staging-01", "deployment": "operation-01"}
    }))
}

/// `env promote --force` sends no expected generation and asks the daemon
/// to skip that precondition.
#[test]
fn promote_force_sends_no_expected_generation() {
    let server = request_server(
        promoted_view(),
        "POST",
        "/api/v1/environments/app-notes-01/promote?force=true",
        json!({"deployment": "operation-01"}),
        promotion_accepted(),
    );
    let output = run(
        &server,
        &[
            "env",
            "promote",
            "app-notes-01",
            "production",
            "--deployment",
            "operation-01",
            "--force",
            "--yes",
            "--no-wait",
        ],
    );
    assert_eq!(assert_json_success(&output)["operation_id"], "operation-02");
    let _ = server.finish();
}

/// `env rollback` saves the environment's rollback by name, and `default`
/// clears it.
#[test]
fn env_rollback_saves_the_setting_or_returns_to_the_default() {
    for (choice, value) in [("manual", json!("manual")), ("default", Value::Null)] {
        assert_saves(
            app_view("app-notes-01", "notes"),
            &[
                "env",
                "rollback",
                "app-notes-01",
                "production",
                choice,
                "--yes",
            ],
            "PUT",
            "environments/production/rollback",
            &json!({ "value": value }),
        );
    }
}

/// `env history` lists deployments with where each came from, naming the
/// failed deployment an automatic rollback rolled back, and `env restore`
/// restores the one it names, reporting the new deployment.
#[test]
fn history_lists_restores_and_restore_names_the_deployment() {
    let server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(two_environment_view()),
        "/api/v1/environments/app-notes-01/deployments" => {
            let mut rollback = deployment("succeeded");
            rollback["operation"]["id"] = json!("operation-03");
            rollback["origin"] = json!({
                "type": "restore", "deployment": "operation-01", "rolled_back": "operation-02"
            });
            let mut restored = deployment("succeeded");
            restored["operation"]["id"] = json!("operation-02");
            restored["current_target"] = json!(false);
            restored["origin"] = json!({"type": "restore", "deployment": "operation-01"});
            Reply::json(page(vec![rollback, restored], None))
        }
        path => panic!("unexpected path {path}"),
    });
    let output = run_human(&server, &["env", "history", "app-notes-01", "production"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        "operation-03  succeeded",
        "(current)",
        "rolled back from deployment operation-02 to deployment operation-01",
        "operation-02  succeeded",
        "restored from deployment operation-01",
    ] {
        assert!(stdout.contains(line), "{line:?} missing from {stdout}");
    }
    assert_eq!(stdout.matches("(current)").count(), 1, "{stdout}");
    let _ = server.finish();

    let server = request_server(
        two_environment_view(),
        "POST",
        "/api/v1/environments/app-notes-01/restore",
        json!({"deployment": "operation-01"}),
        Reply::accepted(json!({
            "operation_id": "operation-02", "environment_id": "app-notes-01", "generation": 1
        })),
    );
    let output = run(
        &server,
        &[
            "env",
            "restore",
            "app-notes-01",
            "production",
            "operation-01",
            "--yes",
            "--no-wait",
        ],
    );
    let report = assert_json_success(&output);
    assert_eq!(report["deployment_id"], "operation-02");
    assert_eq!(report["environment_id"], "app-notes-01");
    let _ = server.finish();
}

/// Where `operation-01`, a deployment of `app-notes-01`, reads alone.
const DEPLOYMENT: &str = "/api/v1/environments/app-notes-01/deployments/operation-01";

/// `operation-01` as a deployment of `app-notes-01` read from `main`.
fn deployment(state: &str) -> Value {
    json!({
        "operation": operation(state),
        "template": app_view("app-notes-01", "notes")["application"],
        "variables": {"git.branch": "main", "git.sha": "a".repeat(40), "env.name": "production"},
        "application": null,
        "release": "rel-notes-01",
        "succeeded_at_ms": null,
        "current_target": true,
        "last_successful": false
    })
}

/// Serves `env wait` for `app-notes-01`: its latest deployment is
/// `operation-01`, whose timeline streams as each of `states` in turn, and
/// its detail as each of `details` in turn (the last one repeats).
fn wait_server(states: Vec<&'static str>, details: Vec<Value>) -> TestServer {
    let state = *states.last().unwrap();
    let timelines: Vec<_> = states
        .into_iter()
        .map(|state| timeline(&operation(state), &json!([])))
        .collect();
    let mut details = details.into_iter();
    let mut current = None;
    start_server(false, usize::MAX, move |request| {
        match request.path.split('?').next().unwrap() {
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/environments/app-notes-01/deployments" => {
                Reply::json(page(vec![deployment(state)], None))
            }
            DEPLOYMENT => Reply::json(deployment(state)),
            TIMELINE_STREAM => Reply::timelines(&timelines),
            "/api/v1/environments/app-notes-01/detail" => {
                current = details.next().or(current.take());
                Reply::json(current.clone().unwrap())
            }
            path => panic!("unexpected path {path}"),
        }
    })
}

/// By default `env wait` returns once the runtime is healthy, with its URL
/// still pending; `--ready routes` also waits for the URL.
#[test]
fn wait_follows_one_deployment_until_it_is_ready() {
    for (ready, url_state) in [(&[][..], "pending"), (&["--ready", "routes"], "ready")] {
        let server = wait_server(
            vec!["running", "succeeded"],
            vec![
                detail("operation-01", false, "pending"),
                detail("operation-01", true, "pending"),
                detail("operation-01", true, "ready"),
            ],
        );
        let arguments = [&["env", "wait", "app-notes-01"][..], ready].concat();
        let output = run_with_timeout(&server, &arguments, "10s");
        let result = assert_json_success(&output);
        server.finish();
        assert_eq!(result["deployment_id"], "operation-01");
        assert_eq!(result["outcome"], "succeeded");
        assert_eq!(result["urls"][0]["state"], url_state, "{ready:?}");
        // Every stderr line is a progress event while waiting.
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            assert!(serde_json::from_str::<Value>(line).unwrap()["progress"].is_object());
        }
    }
}

/// `env wait` reports the local snapshot a restore runs, through restores.
#[test]
fn wait_reports_the_local_snapshot_a_restore_runs() {
    let mut restored = deployment("succeeded");
    restored["origin"] = json!({"type": "restore", "deployment": "operation-00"});
    let mut snapshot = deployment("succeeded");
    snapshot["operation"]["id"] = json!("operation-00");
    snapshot["current_target"] = json!(false);
    snapshot["origin"] = json!({"type": "local", "commit": "c".repeat(40), "base": "b".repeat(40)});
    let server = start_server(false, usize::MAX, move |request| {
        match request.path.split('?').next().unwrap() {
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/environments/app-notes-01/deployments" => {
                Reply::json(page(vec![restored.clone(), snapshot.clone()], None))
            }
            DEPLOYMENT => Reply::json(restored.clone()),
            "/api/v1/environments/app-notes-01/deployments/operation-00" => {
                Reply::json(snapshot.clone())
            }
            TIMELINE_STREAM => Reply::timeline(&operation("succeeded")),
            "/api/v1/environments/app-notes-01/detail" => {
                Reply::json(detail("operation-01", true, "ready"))
            }
            path => panic!("unexpected path {path}"),
        }
    });
    let output = run_with_timeout(&server, &["env", "wait", "app-notes-01"], "10s");
    let result = assert_json_success(&output);
    server.finish();
    assert_eq!(result["deployment_id"], "operation-01");
    assert_eq!(result["local"], true);
    assert_eq!(result["base"], "b".repeat(40));
}

#[test]
fn wait_never_follows_a_newer_deployment_and_fails_or_times_out_with_its_code() {
    let pending = || detail("operation-01", true, "pending");
    // A retry of the deployment that failed after it first succeeded.
    let mut retried = detail("operation-01", true, "ready");
    retried["latest_operation"]["state"] = json!("failed");
    // A daemon that does not report URL readiness never reads as ready.
    let mut no_urls = detail("operation-01", true, "ready");
    no_urls.as_object_mut().unwrap().remove("urls");
    let cases = [
        // Superseded while pending, or after succeeding by a newer one.
        (
            vec!["superseded"],
            pending(),
            "10s",
            3,
            "deployment_superseded",
        ),
        (
            vec!["succeeded"],
            detail("operation-02", true, "pending"),
            "10s",
            3,
            "deployment_superseded",
        ),
        (vec!["failed"], pending(), "10s", 5, "operation_failed"),
        (vec!["succeeded"], retried, "10s", 5, "operation_failed"),
        (vec!["succeeded"], no_urls, "10s", 1, "failed"),
        // Ready only once routes are: `--wait-timeout` ends the local wait.
        (vec!["running"], pending(), "500ms", 4, "timeout"),
    ];
    for (states, detail, timeout, code, error_code) in cases {
        let newer = detail["latest_operation"]["id"] == "operation-02";
        let server = wait_server(states, vec![detail]);
        let output = run(
            &server,
            &[
                "--wait-timeout",
                timeout,
                "env",
                "wait",
                "app-notes-01",
                "--ready",
                "routes",
            ],
        );
        server.finish();
        assert_eq!(output.status.code(), Some(code), "{error_code}");
        let error = support::json_error(&output);
        assert_eq!(error["code"], error_code);
        if newer {
            assert_eq!(error["details"]["superseded_by"], "operation-02");
        }
    }
}

/// A runtime the daemon could not observe is not ready, even for a target
/// with no services to wait for and at every readiness level.
#[test]
fn wait_needs_the_runtime_observed() {
    let mut unobserved = detail("operation-01", true, "ready");
    unobserved["application"]["application"]["spec"]["services"] = json!([]);
    unobserved["observed"]["services"] = json!([]);
    unobserved["diagnostics"] = json!([{
        "code": "runtime_unavailable", "message": "Runtime observation is unavailable."
    }]);
    for ready in [&[][..], &["--ready", "routes"]] {
        let server = wait_server(vec!["succeeded"], vec![unobserved.clone()]);
        let arguments = [
            &["--wait-timeout", "500ms", "env", "wait", "app-notes-01"][..],
            ready,
        ]
        .concat();
        let output = run(&server, &arguments);
        server.finish();
        assert_eq!(output.status.code(), Some(4), "{ready:?}: {output:?}");
        assert_eq!(support::json_error(&output)["code"], "timeout");
    }
}

#[test]
fn deploy_output_carries_the_deployment_fields_agents_need() {
    for no_wait in [true, false] {
        let server = start_server(false, usize::MAX, move |request| {
            match request.path.split('?').next().unwrap() {
                "/api/v1/applications/app-notes-01" => {
                    Reply::json(app_view("app-notes-01", "notes"))
                }
                "/api/v1/environments/app-notes-01/deploy" => {
                    Reply::accepted(accepted("app-notes-01"))
                }
                TIMELINE_STREAM => Reply::timeline(&operation("succeeded")),
                DEPLOYMENT => Reply::json(deployment("succeeded")),
                "/api/v1/environments/app-notes-01/detail" => {
                    Reply::json(detail("operation-01", true, "pending"))
                }
                path => panic!("unexpected path {path}"),
            }
        });
        let mut args = vec!["env", "deploy", "app-notes-01", "--yes"];
        if no_wait {
            args.push("--no-wait");
        }
        let result = assert_json_success(&run(&server, &args));
        server.finish();
        for (field, value) in [
            ("application_id", json!("app-notes-01")),
            ("environment_id", json!("app-notes-01")),
            ("slug", json!("production")),
            ("deployment_id", json!("operation-01")),
            ("operation_id", json!("operation-01")),
        ] {
            assert_eq!(result[field], value, "{field}");
        }
        if no_wait {
            // The accepted operation's fields are kept.
            assert_eq!(result["generation"], 1);
            assert!(result.get("outcome").is_none() && result.get("urls").is_none());
        } else {
            // So are the waited result's.
            assert_eq!(result["accepted"]["operation_id"], "operation-01");
            assert_eq!(result["operation"]["state"], "succeeded");
            assert_eq!(result["outcome"], "succeeded");
            assert_eq!(result["release_id"], "rel-notes-01");
            assert_eq!(result["branch"], "main");
            assert_eq!(result["commit"], "a".repeat(40));
            assert_eq!(result["urls"][0]["url"], "https://notes.example.com");
            assert_eq!(result["urls"][0]["state"], "pending");
        }
    }
}

/// `env stop` waits until the runtime reports its services stopped, fails
/// when the deployment it ran again failed, even before its first poll, and
/// ends when the environment is started meanwhile.
#[test]
fn env_stop_waits_until_stopped_unless_the_stop_fails_or_is_undone() {
    // (attempt after the stop, run state, runtime health at each status
    // read, exit code)
    for (attempt, run_state, healths, code) in [
        (1, "stopped", &["ready", "stopped"][..], 0),
        (2, "stopped", &["ready"][..], 5),
        (1, "running", &["ready"][..], 0),
    ] {
        let mut reads = healths.iter();
        let server = start_server(false, usize::MAX, move |request| {
            match request.path.split('?').next().unwrap() {
                "/api/v1/applications/app-notes-01" => {
                    Reply::json(app_view("app-notes-01", "notes"))
                }
                "/api/v1/environments/app-notes-01/deployments" => {
                    Reply::json(page(vec![deployment("succeeded")], None))
                }
                "/api/v1/environments/app-notes-01/stop" => {
                    Reply::accepted(accepted("app-notes-01"))
                }
                "/api/v1/environments/app-notes-01/status" => {
                    let mut status = status("app-notes-01", "ready");
                    status["runtime_health"] = json!(reads.next().expect("one more status read"));
                    Reply::json(status)
                }
                "/api/v1/environments/app-notes-01" => {
                    let mut environment = environment("app-notes-01", "app-notes-01", "production");
                    environment["run_state"] = json!(run_state);
                    Reply::json(environment)
                }
                "/api/v1/operations/operation-01" => {
                    let mut operation = operation(if attempt > 1 { "failed" } else { "succeeded" });
                    operation["attempt"] = json!(attempt);
                    Reply::json(operation)
                }
                path => panic!("unexpected path {path}"),
            }
        });
        let output = run(&server, &["env", "stop", "app-notes-01", "--yes"]);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status_reads = server
            .finish()
            .iter()
            .filter(|request| request.path == "/api/v1/environments/app-notes-01/status")
            .count();
        assert_eq!(status_reads, healths.len());
    }
}
