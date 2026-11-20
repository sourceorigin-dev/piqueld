//! Field edits that save one part of an application's configuration.

use super::*;

#[test]
fn field_edit_sends_only_the_selected_value_and_defaults_to_save_only() {
    for (arguments, suffix, expected) in [
        (
            vec![
                "app",
                "service",
                "replicas",
                "app-notes-01",
                "web",
                "3",
                "--yes",
            ],
            "replicas",
            json!({"value":3}),
        ),
        (
            vec![
                "app",
                "service",
                "env",
                "set",
                "app-notes-01",
                "web",
                "GREETING",
                "hello world",
                "--yes",
            ],
            "environment/GREETING",
            json!({"value":"hello world"}),
        ),
        (
            vec![
                "app",
                "service",
                "arguments",
                "app-notes-01",
                "web",
                "--yes",
                "--",
                "--flag",
                "arg with spaces",
            ],
            "arguments",
            json!({"value":["--flag","arg with spaces"]}),
        ),
        (
            vec![
                "app",
                "service",
                "cpu",
                "app-notes-01",
                "web",
                "--clear",
                "--yes",
            ],
            "resources/cpu",
            json!({"value":null}),
        ),
    ] {
        assert_saves_field(&arguments, suffix, &expected);
    }
}

/// [`assert_saves`] for the `notes` application's `web` service field `suffix`.
fn assert_saves_field(arguments: &[&str], suffix: &str, expected: &Value) {
    let view = app_view("app-notes-01", "notes");
    assert_saves(
        view,
        arguments,
        "PUT",
        &format!("services/web/{suffix}"),
        expected,
    );
}

/// Runs `arguments` against a server that serves `notes`, asserting they
/// exit as an input error after only reading it, as editing an entry the
/// application lacks does.
fn assert_edit_refused(arguments: &[&str]) {
    let server = start_server(false, 1, |_| Reply::json(app_view("app-notes-01", "notes")));
    assert_eq!(run(&server, arguments).status.code(), Some(2));
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn file_add_inlines_local_files_and_names_repository_files() {
    let directory = tempdir().expect("file directory");
    let local = directory.path().join("prometheus.yml");
    std::fs::write(&local, "scrape: ${HOME} {{ .x }}\n").expect("local file");
    let local = local.to_str().expect("UTF-8 path");
    // Local files are inlined verbatim, never rendered.
    assert_saves_field(
        &[
            "app",
            "file",
            "add",
            "app-notes-01",
            "web",
            "/etc/prometheus/prometheus.yml",
            "--from",
            local,
            "--yes",
        ],
        "file",
        &json!({"target":"/etc/prometheus/prometheus.yml","content":"scrape: ${HOME} {{ .x }}\n"}),
    );
    assert_saves_field(
        &[
            "app",
            "file",
            "add",
            "app-notes-01",
            "web",
            "/etc/otelcol/config.yaml",
            "--path",
            "otel/config.yaml",
            "--mode",
            "640",
            "--yes",
        ],
        "file",
        &json!({"target":"/etc/otelcol/config.yaml","path":"otel/config.yaml","mode":"0640"}),
    );
}

#[test]
fn job_commands_edit_the_loaded_jobs_and_save_the_whole_list() {
    let job = |name: &str, command: &[&str], timeout: u32| json!({"name": name, "service": "web", "command": command, "run": "before-rollout", "timeout_seconds": timeout});
    for (arguments, expected) in [
        // Replacing keeps the job's position and, without --timeout, its timeout.
        (
            vec![
                "app",
                "job",
                "set",
                "app-notes-01",
                "migrate",
                "web",
                "--yes",
                "--",
                "notes",
                "migrate",
                "--all",
            ],
            json!([
                job("migrate", &["notes", "migrate", "--all"], 60),
                job("seed", &["notes", "seed"], 300)
            ]),
        ),
        (
            vec![
                "app",
                "job",
                "set",
                "app-notes-01",
                "cleanup",
                "web",
                "--timeout-seconds",
                "30",
                "--yes",
                "--",
                "notes",
                "cleanup",
            ],
            json!([
                job("migrate", &["notes", "migrate"], 60),
                job("seed", &["notes", "seed"], 300),
                job("cleanup", &["notes", "cleanup"], 30)
            ]),
        ),
        // Moving reorders without changing the job; past the end moves it last.
        (
            vec![
                "app",
                "job",
                "move",
                "app-notes-01",
                "migrate",
                "9",
                "--yes",
            ],
            json!([
                job("seed", &["notes", "seed"], 300),
                job("migrate", &["notes", "migrate"], 60)
            ]),
        ),
        (
            vec!["app", "job", "remove", "app-notes-01", "seed", "--yes"],
            json!([job("migrate", &["notes", "migrate"], 60)]),
        ),
    ] {
        let mut view = app_view("app-notes-01", "notes");
        view["application"]["spec"]["jobs"] = json!([
            job("migrate", &["notes", "migrate"], 60),
            job("seed", &["notes", "seed"], 300)
        ]);
        assert_saves(view, &arguments, "PUT", "jobs", &json!({"value": expected}));
    }

    assert_edit_refused(&["app", "job", "remove", "app-notes-01", "seed", "--yes"]);
}

#[test]
fn secret_mount_commands_edit_the_loaded_mounts_and_save_the_whole_list() {
    let mount = |name: &str, target: &str| json!({"name": name, "target": target});
    for (arguments, expected) in [
        // Adding at a mounted target replaces that mount in place.
        (
            vec!["add", "stripe-live", "/run/secrets/db"],
            json!([
                mount("stripe-live", "/run/secrets/db"),
                mount("session", "/run/secrets/session")
            ]),
        ),
        (
            vec!["add", "${{ vars.stripe }}", "/run/secrets/stripe"],
            json!([
                mount("database", "/run/secrets/db"),
                mount("session", "/run/secrets/session"),
                mount("${{ vars.stripe }}", "/run/secrets/stripe")
            ]),
        ),
        (
            vec!["remove", "/run/secrets/db"],
            json!([mount("session", "/run/secrets/session")]),
        ),
    ] {
        let mut view = app_view("app-notes-01", "notes");
        view["application"]["spec"]["services"][0]["secrets"] = json!([
            mount("database", "/run/secrets/db"),
            mount("session", "/run/secrets/session")
        ]);
        let (action, values) = arguments.split_first().unwrap();
        let mut arguments = vec!["app", "service", "secret", action, "app-notes-01", "web"];
        arguments.extend(values);
        arguments.push("--yes");
        assert_saves(
            view,
            &arguments,
            "PUT",
            "services/web/secrets",
            &json!({"value": expected}),
        );
    }

    assert_edit_refused(&[
        "app",
        "service",
        "secret",
        "remove",
        "app-notes-01",
        "web",
        "/run/secrets/db",
        "--yes",
    ]);
}

#[test]
fn link_commands_edit_the_loaded_links_and_save_the_whole_list() {
    let saved_export = json!({"service": "web", "applications": ["billing"]});
    let saved_import = json!({"application": "auth", "service": "api"});
    for (arguments, field, expected) in [
        // Setting an exported service replaces who may import it.
        (
            vec![
                "app",
                "export",
                "set",
                "app-notes-01",
                "web",
                "--applications",
                "auth,billing",
                "--previews",
                "--yes",
            ],
            "exports",
            json!([{"service": "web", "applications": ["auth", "billing"], "previews": true}]),
        ),
        // Importing again replaces the environment it reaches.
        (
            vec![
                "app",
                "import",
                "add",
                "app-notes-01",
                "auth",
                "api",
                "--environment",
                "staging",
                "--yes",
            ],
            "imports",
            json!([{"application": "auth", "service": "api", "environment": "staging"}]),
        ),
        (
            vec![
                "app",
                "import",
                "add",
                "app-notes-01",
                "billing",
                "invoices",
                "--yes",
            ],
            "imports",
            json!([saved_import.clone(), {"application": "billing", "service": "invoices"}]),
        ),
        (
            vec![
                "app",
                "import",
                "remove",
                "app-notes-01",
                "auth",
                "api",
                "--yes",
            ],
            "imports",
            json!([]),
        ),
    ] {
        let mut view = app_view("app-notes-01", "notes");
        view["application"]["spec"]["exports"] = json!([saved_export.clone()]);
        view["application"]["spec"]["imports"] = json!([saved_import.clone()]);
        assert_saves(view, &arguments, "PUT", field, &json!({"value": expected}));
    }

    assert_edit_refused(&[
        "app",
        "import",
        "remove",
        "app-notes-01",
        "auth",
        "api",
        "--yes",
    ]);
}

#[test]
fn generated_secret_commands_edit_the_loaded_declarations_and_save_the_whole_list() {
    let saved =
        json!({"name": "session", "generate": {"type": "random", "bytes": 32, "encoding": "hex"}});
    for (arguments, expected) in [
        // Setting a declared name replaces how it is generated, in place.
        (
            vec![
                "app",
                "generated-secret",
                "set",
                "app-notes-01",
                "session",
                "--bytes",
                "64",
                "--encoding",
                "base64url",
                "--yes",
            ],
            json!([{"name": "session", "generate": {"type": "random", "bytes": 64, "encoding": "base64url"}}]),
        ),
        (
            vec![
                "app",
                "generated-secret",
                "set",
                "app-notes-01",
                "signing-key",
                "--rsa-bits",
                "3072",
                "--yes",
            ],
            json!([saved.clone(), {"name": "signing-key", "generate": {"type": "rsa", "bits": 3072}}]),
        ),
        (
            vec![
                "app",
                "generated-secret",
                "remove",
                "app-notes-01",
                "session",
                "--yes",
            ],
            json!([]),
        ),
    ] {
        let mut view = app_view("app-notes-01", "notes");
        view["application"]["spec"]["secrets"] = json!([saved.clone()]);
        assert_saves(
            view,
            &arguments,
            "PUT",
            "generated-secrets",
            &json!({"value": expected}),
        );
    }

    // A generator is required, and `--encoding` applies only to random bytes.
    for arguments in [
        &[
            "app",
            "generated-secret",
            "set",
            "app-notes-01",
            "session",
            "--yes",
        ][..],
        &[
            "app",
            "generated-secret",
            "set",
            "app-notes-01",
            "session",
            "--rsa-bits",
            "2048",
            "--encoding",
            "hex",
            "--yes",
        ],
    ] {
        let server = start_server(false, 0, |_| unreachable!("rejected before connecting"));
        assert_eq!(run(&server, arguments).status.code(), Some(2));
        server.finish();
    }

    assert_edit_refused(&[
        "app",
        "generated-secret",
        "remove",
        "app-notes-01",
        "session",
        "--yes",
    ]);
}

#[test]
fn field_edit_deploy_is_explicit_and_no_wait_returns_the_saved_receipt() {
    let server = start_server(false, 2, |request| {
        if request.method == "GET" {
            return Reply::json(app_view("app-notes-01", "notes"));
        }
        assert_eq!(
            request.path,
            "/api/v1/applications/app-notes-01/services/web/replicas?deploy=true&expected_generation=1"
        );
        Reply::accepted(
            json!({"application_id":"app-notes-01","generation":2,"operation_id":"operation-01"}),
        )
    });
    let output = run(
        &server,
        &[
            "app",
            "service",
            "replicas",
            "app-notes-01",
            "web",
            "3",
            "--deploy",
            "--no-wait",
            "--yes",
        ],
    );
    assert_eq!(assert_json_success(&output)["operation_id"], "operation-01");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn git_service_creation_does_not_require_a_placeholder_image() {
    let server = start_server(false, 2, |request| {
        if request.method == "GET" {
            return Reply::json(app_view("app-notes-01", "notes"));
        }
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.path,
            "/api/v1/applications/app-notes-01/services?deploy=false&expected_generation=1"
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["name"], "worker");
        assert_eq!(
            body["source"],
            json!({"type":"git","repository":{"url":"https://example.com/app.git","branch":"release"},"build":{"type":"docker","dockerfile":"build/Dockerfile","context":"build","args":{"ORIGIN":"https://a=b"},"target":"runtime"}})
        );
        Reply::json(saved())
    });
    let output = run(
        &server,
        &[
            "app",
            "service",
            "add",
            "app-notes-01",
            "worker",
            "--git",
            "https://example.com/app.git",
            "--branch",
            "release",
            "--dockerfile",
            "build/Dockerfile",
            "--context",
            "build",
            "--build-arg",
            "ORIGIN=https://a=b",
            "--target",
            "runtime",
            "--yes",
        ],
    );
    assert_eq!(assert_json_success(&output)["generation"], 2);
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn self_git_sources_build_the_manifest_repository() {
    assert_saves_field(
        &[
            "app",
            "service",
            "source",
            "git",
            "app-notes-01",
            "web",
            "self",
            "--dockerfile",
            "build/Dockerfile",
            "--yes",
        ],
        "source",
        &json!({"value":{"type":"git","repository":"self","build":{"type":"docker","dockerfile":"build/Dockerfile","context":"."}}}),
    );
    // `self` always builds the manifest's commit, so a revision is rejected
    // before contacting the daemon.
    let server = start_server(false, 0, |_| {
        panic!("`self` with a branch must fail locally")
    });
    let output = run(
        &server,
        &[
            "app",
            "service",
            "add",
            "app-notes-01",
            "worker",
            "--git",
            "self",
            "--branch",
            "release",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("do not apply to `self`"));
    let _ = server.finish();
}

// Runs over TCP so the global `--url` endpoint and the positional repository
// URL are both given.
#[test]
fn repository_connect_sends_the_repository_without_replacing_the_endpoint() {
    assert_saves(
        app_view("app-notes-01", "notes"),
        &[
            "app",
            "repository",
            "connect",
            "app-notes-01",
            "https://example.com/app.git",
            "piquel.toml",
            "--branch",
            "release",
            "--yes",
        ],
        "PUT",
        "repository",
        &json!({"value":{"repository":{"url":"https://example.com/app.git","branch":"release"},"path":"piquel.toml"}}),
    );
}

#[test]
fn sync_settings_and_environment_opt_outs_are_sent_as_typed_requests() {
    let mut view = app_view("app-notes-01", "notes");
    view["application"]["spec"]["manifest"] = json!({
        "repository": {"url": "https://example.com/notes.git", "branch": "main"},
        "path": "app.toml"
    });
    assert_saves(
        view,
        &[
            "app",
            "repository",
            "sync",
            "app-notes-01",
            "poll",
            "--interval",
            "120",
            "--yes",
        ],
        "PUT",
        "repository/sync",
        &json!({"value": {"mode": "poll", "interval_seconds": 120}}),
    );

    // An interval only applies to polling, and nothing is sent.
    let server = start_server(false, 0, |request| panic!("unexpected {}", request.path));
    let output = run(
        &server,
        &[
            "app",
            "repository",
            "sync",
            "app-notes-01",
            "webhook",
            "--interval",
            "120",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--interval applies only to `poll`"));
    assert!(server.finish().is_empty());

    let mut staging = environment("app-notes-01", "env-staging-01", "staging");
    staging["sync"] = json!(false);
    let server = request_server(
        two_environment_view(),
        "PUT",
        "/api/v1/environments/env-staging-01/sync",
        json!({"enabled": false}),
        Reply::json(staging),
    );
    let output = run(&server, &["env", "sync", "app-notes-01", "staging", "off"]);
    assert_eq!(assert_json_success(&output)["sync"], false);
    let _ = server.finish();
}

#[test]
fn telemetry_receivers_and_grants_are_sent_as_typed_requests() {
    assert_saves(
        app_view("app-notes-01", "notes"),
        &[
            "app",
            "service",
            "telemetry",
            "app-notes-01",
            "web",
            "on",
            "--yes",
        ],
        "PUT",
        "services/web/receives-telemetry",
        &json!({"value": true}),
    );
    let mut staging = environment("app-notes-01", "env-staging-01", "staging");
    staging["telemetry"] = json!(true);
    let server = request_server(
        two_environment_view(),
        "PUT",
        "/api/v1/environments/env-staging-01/telemetry",
        json!({"granted": true}),
        Reply::json(staging),
    );
    let output = run(
        &server,
        &["env", "telemetry", "app-notes-01", "staging", "on"],
    );
    assert_eq!(assert_json_success(&output)["telemetry"], true);
    let _ = server.finish();
}
