//! Daemon status, resources, settings, diagnostics, events, analytics and completions.

use super::*;

#[test]
fn status_works_over_tcp_and_unix_with_clean_json() {
    for unix in [false, true] {
        let server = start_server(unix, 1, |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/v1/system/status");
            Reply::json(json!({
                "status": "running",
                "api_version": "v1",
                "daemon_version": "0.1.0",
                "instance_id": "instance-test"
            }))
        });
        let output = run(&server, &["status"]);
        let value = assert_json_success(&output);
        assert_eq!(value["instance_id"], "instance-test");
        assert_eq!(output.stderr, b"");
        let _ = server.finish();
    }
}

/// A daemon setting as the API reports it.
fn setting(value: u64, source: &str) -> Value {
    json!({
        "key": "retention.event_days", "section": "History",
        "label": "Application events (days)",
        "description": "Days application events are kept.",
        "value": value, "default": 90, "source": source,
        "kind": {"type": "integer", "minimum": 0, "nullable": false}
    })
}

#[test]
fn resources_and_settings_show_the_daemon_measurements_and_settings() {
    let server = start_server(false, 4, |request| match request.path.as_str() {
        "/api/v1/system/settings/retention.event_days" => {
            assert_eq!(request.method, "PUT");
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).expect("the request body is JSON"),
                json!({"value": 30})
            );
            Reply::json(setting(30, "changed"))
        }
        "/api/v1/system/resources" => Reply::json(json!({
            "sampled_at_ms": 1_700_000_000_000_i64, "uptime_seconds": 273_600,
            "memory_bytes": 50_331_648, "cpu_seconds": 12.5, "cpu_percent": null,
            "database_bytes": 12_582_912, "wal_bytes": 4_194_304,
            "available_disk_bytes": null, "events": 1234, "diagnostics": 5,
            "build_output_bytes": 2048, "running_operations": 1, "queued_operations": 2,
            "pending_deliveries": 3, "failed_deliveries": 0,
            "oldest_queued_operation_ms": 125_000, "oldest_pending_delivery_ms": null,
            "waiting_writes": 1, "write_waits": 12, "write_wait_ms": 340
        })),
        "/api/v1/system/configuration" => Reply::json(json!({
            "groups": {
                "Docker": {"Socket": "/run/docker.sock"},
                "Server": {"HTTP listen mode": "unix", "HTTP port": "7845"}
            },
            "settings": [setting(90, "default")]
        })),
        path => panic!("unexpected request to {path}"),
    });
    let output = run_human(&server, &["resources"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        "Uptime: 3 d 4 h",
        "Memory: 48.0 MiB resident",
        "CPU: not measured yet; ask again in 5 seconds",
        "Database: 12.0 MiB (WAL 4.0 MiB)",
        "Available disk: unavailable on this host",
        "Retained build output: 2.0 KiB",
        "Operations: 1 running, 2 queued; the oldest has waited 2 min 5 s\n",
        "Notification deliveries: 3 awaiting a retry, 0 failed\n",
        "Database writes: 1 waiting; 12 waited 340 ms in all\n",
    ] {
        assert!(stdout.contains(line), "{stdout}");
    }

    let output = run_human(&server, &["settings"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Docker (configuration file only):\n  Socket: /run/docker.sock\n\n\
         Server (configuration file only):\n  HTTP listen mode: unix\n  HTTP port: 7845\n\n\
         History:\n  retention.event_days: 90 (default)\n"
    );
    let settings = assert_json_success(&run(&server, &["settings", "list"]));
    assert_eq!(settings["groups"]["Server"]["HTTP port"], "7845");
    let output = run_human(&server, &["settings", "set", "retention.event_days", "30"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with(
            "retention.event_days: 30\n  Name: Application events (days)\n  Source: changed\n"
        ),
        "{stdout}"
    );
    assert_eq!(server.finish().len(), 4);
}

#[test]
fn doctor_reports_the_connection_and_exits_6_on_a_failed_check() {
    for (status, code) in [("warn", 0), ("fail", 6)] {
        let server = start_server(true, 1, move |request| {
            assert_eq!(request.path, "/api/v1/system/checks");
            Reply::json(json!({"checks": [
                {"id": "public_url", "status": "pass", "message": "reachable", "next_step": null},
                {"id": "secret_key", "status": status, "message": "unusable", "next_step": "restore it"},
            ]}))
        });
        let output = run(&server, &["doctor"]);
        assert_eq!(output.status.code(), Some(code));
        let value: Value = serde_json::from_slice(&output.stdout).expect("clean JSON stdout");
        assert_eq!(value["connection"]["endpoint_source"], "flag --socket");
        assert_eq!(value["checks"][1]["status"], status);
        if code == 6 {
            let error: Value = serde_json::from_slice(&output.stderr).expect("error event");
            assert_eq!(error["error"]["code"], "checks_failed");
        }
        let _ = server.finish();
    }
}

#[test]
fn doctor_reports_the_connection_when_the_daemon_times_out() {
    let (release, pending) = mpsc::channel::<()>();
    let server = start_server(true, 1, move |_| {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the reply is only held back until the CLI gives up or the test releases it"
        )]
        let _ = pending.recv_timeout(READ_TIMEOUT);
        Reply::json(json!({"checks": []}))
    });
    let output = run_with_timeout(&server, &["doctor"], "500ms");
    drop(release);
    assert_eq!(output.status.code(), Some(4));
    let value: Value = serde_json::from_slice(&output.stdout).expect("clean JSON stdout");
    assert_eq!(value["connection"]["credential"], "none");
    assert_eq!(value["checks"], Value::Null);
    let _ = server.finish();
}

/// The host operator's socket commands never send a credential, which the
/// daemon would honour instead of the operator, even with `PIQUELD_TOKEN`.
#[test]
fn recover_admin_sends_no_credential() {
    let link = json!({"url": "https://piqueld.example/dashboard/auth#invite=x", "expires_at": 1});
    let server = start_server(true, 1, move |_| Reply::bare(&link));
    let output = server
        .command()
        .arg("recover-admin")
        .env("PIQUELD_TOKEN", "pqd_saved")
        .output()
        .expect("piquelctl output");
    assert!(output.status.success(), "{output:?}");
    let requests = server.finish();
    assert_eq!(requests[0].path, "/api/v1/auth/recovery");
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[test]
fn events_cli_reads_a_filtered_page_newest_first() {
    let server = start_server(false, 1, |request| {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            "/api/v1/events?cursor=v1%3A7&descending=true&environment_id=app-notes-01&limit=2"
        );
        Reply::json(
            json!({"items":[{"id":8,"environment_id":"app-notes-01","operation_id":"operation-01","generation":1,"attempt":2,"kind":"operation_succeeded","message":null,"error_code":null,"phase":null,"resource":null,"created_at_ms":123}],"next_cursor":null}),
        )
    });
    let output = run(
        &server,
        &[
            "event",
            "--env",
            "app-notes-01",
            "--cursor",
            "v1:7",
            "--limit",
            "2",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("the json decodes");
    assert_eq!(result["items"][0]["attempt"], 2);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn completions_print_a_shell_script() {
    let output = support::command()
        .args(["completions", "bash"])
        .output()
        .expect("piquelctl process");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("_piquelctl"));
}

#[test]
fn analytics_reads_one_environment_over_a_relative_window() {
    let server = start_server(false, 2, |request| {
        if request.path == "/api/v1/applications/app-notes-01" {
            return Reply::json(two_environment_view());
        }
        let query = request
            .path
            .strip_prefix("/api/v1/analytics/deployments?")
            .expect("an analytics request");
        let parameters: BTreeMap<_, _> = query
            .split('&')
            .filter_map(|parameter| parameter.split_once('='))
            .collect();
        assert_eq!(parameters["environment_id"], "env-staging-01");
        let bound = |name: &str| {
            parameters[name]
                .parse::<i64>()
                .expect("the parameter is a number")
        };
        assert_eq!(bound("until_ms") - bound("since_ms"), 6 * 86_400_000);
        Reply::json(json!({
            "since_ms": 1, "until_ms": 2, "history_started_at_ms": 5, "pruned_through_ms": null,
            "incomplete": true, "deployments": 3, "succeeded": 2, "failed": 1,
            "failed_attempts": 2, "retry_attempts": 1, "action_retries": 0,
            "mean_duration_ms": 65_400.0,
            "actions": [{"phase": "resolving_image", "count": 4, "mean_ms": 12_340.0}],
            "failures": [{"code": "image_pull_failed", "count": 1}]
        }))
    });
    let arguments = [
        "analytics",
        "--app",
        "app-notes-01",
        "--env",
        "staging",
        "--since",
        "7d",
        "--until",
        "1d",
    ];
    let output = run_human(&server, &arguments);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        "Environment: notes / staging",
        "Deployments: 3 (2 succeeded, 1 failed by their latest attempt)",
        "Mean attempt duration: 1 min 5 s",
        "PHASE            ACTIONS  MEAN DURATION\nresolving_image  4        12.3 s\n",
        "image_pull_failed  1",
    ] {
        assert!(stdout.contains(line), "{stdout}");
    }
    assert!(
        stderr.contains("reaches before retained history (recorded from 1970-01-01 00:00:00Z)"),
        "{stderr}"
    );
    assert_eq!(server.finish().len(), 2);

    let server = start_server(false, 0, |_| panic!("an invalid window is not sent"));
    let output = run(&server, &["analytics", "--since", "1d", "--until", "7d"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(server.finish().is_empty());
}
