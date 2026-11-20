//! Operation progress, timelines and the waits that follow them.

use super::*;

#[test]
fn progress_is_visible_before_the_server_completes_the_command() {
    let (release, wait) = mpsc::channel();
    let mut calls = 0;
    // The first stream ends after one timeline; the client reconnects.
    let server = start_server(false, 2, move |request| {
        assert_eq!(request.path, TIMELINE_STREAM);
        calls += 1;
        if calls == 1 {
            let steps = json!([{
                "phase": "resolving_image", "resource": "web", "state": "running",
                "action_id": "a1", "started_at_ms": 1
            }]);
            Reply::timelines(&[timeline(&operation("running"), &steps)])
        } else {
            wait.recv_timeout(Duration::from_secs(5))
                .expect("release final response");
            Reply::timeline(&operation("succeeded"))
        }
    });
    let mut child = server
        .command()
        .args(["--json", "--timeout", "10s", "operation", "operation-01"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (visible, observed) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if line.unwrap().contains("resolving_image") {
                visible.send(()).unwrap();
            }
        }
    });
    let streamed = observed.recv_timeout(Duration::from_secs(3));
    let still_running = child.try_wait().unwrap().is_none();
    release.send(()).unwrap();
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    server.finish();
    streamed.expect("progress must be flushed before releasing the final response");
    assert!(still_running);
    assert_eq!(assert_json_success(&output)["state"], "succeeded");
}

/// Streams a deployment's timeline: a build that took unusually long, then
/// an update that retried once, each timeline sent whole as the daemon does.
fn stepped_timelines() -> Vec<Value> {
    let step = |phase: &str, state: &str, action: &str, started: i64| {
        json!({
            "phase": phase, "resource": "api", "state": state,
            "action_id": action, "started_at_ms": started
        })
    };
    let running = operation("running");
    let mut build = step("building_git", "running", "a1", 1_000);
    build["expected_ms"] = json!(25_000);
    let update = json!({"phase": "ensure_service", "resource": "api", "state": "planned", "expected_ms": 2_000});
    let first = timeline(&running, &json!([build, update]));
    build["state"] = json!("succeeded");
    build["finished_at_ms"] = json!(39_200);
    let mut retrying = step("ensure_service", "retrying", "a2", 39_200);
    retrying["expected_ms"] = json!(2_000);
    retrying["diagnostic"] = json!({
        "id": "d1", "code": "docker_unavailable", "summary": "Docker is unavailable",
        "causes": [], "retryable": true, "next_action": "", "scope": "application"
    });
    let mut updated = retrying.clone();
    updated["state"] = json!("succeeded");
    updated["retries"] = json!(1);
    updated["finished_at_ms"] = json!(41_200);
    updated.as_object_mut().unwrap().remove("diagnostic");
    let mut succeeded = operation("succeeded");
    succeeded["started_at_ms"] = json!(1_000);
    succeeded["finished_at_ms"] = json!(42_000);
    vec![
        first.clone(),
        // Unchanged steps print nothing again.
        first,
        timeline(&running, &json!([build, retrying])),
        timeline(&succeeded, &json!([build, updated])),
    ]
}

/// Without a terminal, each step prints a line when it starts or changes,
/// then the outcome with how long the deployment took and usually takes.
#[test]
fn non_terminal_progress_prints_a_line_per_step_change() {
    let server = start_server(true, 1, |request| {
        assert_eq!(request.path, TIMELINE_STREAM);
        Reply::timelines(&stepped_timelines())
    });
    let output = run_human(&server, &["operation", "operation-01"]);
    server.finish();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "operation-01: running\n\
         operation-01: Build api · running\n\
         operation-01: Build api · succeeded · 38.2 s  (usual 25.0 s)\n\
         operation-01: Update service api · retrying · retry 1: Docker is unavailable\n\
         operation-01: Update service api · succeeded · 2.0 s  after 1 retry\n\
         operation-01: succeeded · 41.0 s (usual 27.0 s)\n"
    );
}

/// Without a terminal, a rollout's step prints a line as its new tasks run,
/// and its last line says how long no task served.
#[test]
fn non_terminal_progress_prints_a_rollouts_new_tasks_and_downtime() {
    // A stop-first rollout: the old task stops at 2 s, the new one runs at 5.2 s.
    let lane = |id: &str, current: bool, spans: &[(&str, i64)]| {
        let spans: Vec<_> = spans
            .iter()
            .map(|(state, from)| json!({"state": state, "from_ms": from}))
            .collect();
        let state = spans.last().unwrap()["state"].clone();
        json!({"id": id, "slot": 1, "current": current, "state": state, "spans": spans})
    };
    let wait = |state: &str, tasks: Value| {
        json!({
            "phase": "wait_for_service", "resource": "web", "state": state,
            "action_id": "a1", "started_at_ms": 1_000, "tasks": tasks, "monitor_seconds": 5
        })
    };
    let old = lane("old", false, &[("running", 1_000), ("shutdown", 2_000)]);
    let running = operation("running");
    let mut succeeded = operation("succeeded");
    succeeded["finished_at_ms"] = json!(9_000);
    let mut done = wait(
        "succeeded",
        json!([
            old,
            lane("new", true, &[("preparing", 2_000), ("running", 5_200)])
        ]),
    );
    done["finished_at_ms"] = json!(9_000);
    let timelines = [
        timeline(
            &running,
            &json!([wait(
                "running",
                json!([old, lane("new", true, &[("preparing", 2_000)])])
            )]),
        ),
        timeline(&running, &json!([wait("running", done["tasks"].clone())])),
        timeline(&succeeded, &json!([done])),
    ];
    let server = start_server(true, 1, move |_| Reply::timelines(&timelines));
    let output = run_human(&server, &["operation", "operation-01"]);
    server.finish();
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "operation-01: running\n\
         operation-01: Wait for service web · running · 0/1 new task running\n\
         operation-01: Wait for service web · running · 1/1 new task running\n\
         operation-01: Wait for service web · succeeded · 8.0 s  down 3.2 s\n\
         operation-01: succeeded · 9.0 s\n"
    );
}

/// With `--json`, each step change is a `step` event carrying the step, and
/// stdout keeps only the result.
#[test]
fn json_progress_emits_a_step_event_per_step_change() {
    let server = start_server(true, 1, |_| Reply::timelines(&stepped_timelines()));
    let output = run(&server, &["operation", "operation-01"]);
    server.finish();
    assert_eq!(assert_json_success(&output)["state"], "succeeded");
    let events: Vec<Value> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let steps: Vec<_> = events
        .iter()
        .filter_map(|event| event.get("step"))
        .map(|step| {
            assert_eq!(step["task"], "operation-01");
            (
                step["phase"].as_str().unwrap(),
                step["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("building_git", "running"),
            ("building_git", "succeeded"),
            ("ensure_service", "retrying"),
            ("ensure_service", "succeeded"),
        ]
    );
    assert_eq!(
        events.last().unwrap()["progress"],
        json!({"task": "operation-01", "message": "41.0 s (usual 27.0 s)", "outcome": "succeeded"})
    );
}

#[test]
fn operation_follows_its_timeline_by_default_and_no_wait_fetches_once() {
    let server = start_server(false, 1, move |request| {
        assert_eq!(request.path, TIMELINE_STREAM);
        Reply::timelines(&[
            timeline(&operation("requested"), &json!([])),
            timeline(&operation("succeeded"), &json!([])),
        ])
    });
    let output = run(&server, &["operation", "operation-01"]);
    let value = assert_json_success(&output);
    assert_eq!(value["state"], "succeeded");
    let _ = server.finish();

    let server = start_server(false, 1, move |request| {
        assert_eq!(request.path, "/api/v1/operations/operation-01");
        Reply::json(operation("requested"))
    });
    let output = run(&server, &["operation", "operation-01", "--no-wait"]);
    let value = assert_json_success(&output);
    assert_eq!(value["state"], "requested");
    let _ = server.finish();
}

#[test]
fn waits_ride_out_an_unavailable_daemon_and_retried_failures() {
    let mut reads = 0;
    let server = start_server(false, 6, move |request| {
        reads += 1;
        match (reads, request.path.as_str()) {
            // The stream ends as the daemon restarts…
            (1, TIMELINE_STREAM) => Reply::timeline(&operation("running")),
            // …which then refuses it, so the timeline is polled instead…
            (2, TIMELINE_STREAM) => Reply::status(
                "503 Service Unavailable",
                &json!({"code": "unavailable", "message": "too many streams"}),
            ),
            // …through a gateway error while it starts.
            (3, "/api/v1/operations/operation-01/timeline") => Reply::status(
                "502 Bad Gateway",
                &json!({"code": "unavailable", "message": "restarting"}),
            ),
            (4, "/api/v1/operations/operation-01/timeline") => {
                let mut failed = operation("failed");
                failed["error_code"] = json!("swarm_manager_unavailable");
                Reply::json(timeline(&failed, &json!([])))
            }
            // Still the latest operation, so the daemon retries its failure.
            (5, "/api/v1/environments/app-notes-01/detail") => {
                Reply::json(detail("operation-01", true, "ready"))
            }
            (6, "/api/v1/operations/operation-01/timeline") => {
                Reply::json(timeline(&operation("succeeded"), &json!([])))
            }
            (read, path) => panic!("unexpected read {read} of {path}"),
        }
    });
    // `--timeout` bounds each request, not the wait.
    let output = run_with_timeout(&server, &["operation", "operation-01"], "100ms");
    let value = assert_json_success(&output);
    assert_eq!(value["state"], "succeeded");
    let _ = server.finish();
}

#[test]
fn a_retryable_failure_is_waited_on_only_while_it_is_the_latest_operation() {
    for (latest, exit, code) in [
        // The daemon keeps retrying: the wait times out, naming the failure.
        ("operation-01", 4, "timeout"),
        // A newer operation replaced it, so nothing retries it.
        ("operation-02", 5, "operation_failed"),
    ] {
        let server = start_server(false, usize::MAX, move |request| {
            match request.path.as_str() {
                // Each stream ends, and is reopened, while the wait goes on.
                TIMELINE_STREAM => {
                    let mut failed = operation("failed");
                    failed["error_code"] = json!("swarm_manager_unavailable");
                    Reply::timeline(&failed)
                }
                "/api/v1/environments/app-notes-01/detail" => {
                    Reply::json(detail(latest, true, "ready"))
                }
                path => panic!("unexpected path {path}"),
            }
        });
        let output = run(
            &server,
            &["--wait-timeout", "1s", "operation", "operation-01"],
        );
        server.stop();
        assert_eq!(output.status.code(), Some(exit), "{output:?}");
        let error = support::json_error(&output);
        assert_eq!(error["code"], code);
        if exit == 4 {
            assert_eq!(
                error["details"]["last_error_code"],
                "swarm_manager_unavailable"
            );
        }
    }
}

#[test]
fn a_wait_that_never_reached_the_daemon_fails_at_once() {
    let started = std::time::Instant::now();
    let output = support::command()
        .args(["--json", "--socket", "/nonexistent/piqueld.sock"])
        .args(["operation", "operation-01"])
        .output()
        .expect("piquelctl process");
    assert!(started.elapsed() < Duration::from_secs(10), "{output:?}");
    assert_eq!(output.status.code(), Some(4));
    let error = support::json_error(&output);
    assert_eq!(error["code"], "connection_failed");
    assert_eq!(error["details"]["endpoint_source"], "flag --socket");
}

#[test]
fn timeout_and_ctrl_c_end_only_the_local_wait() {
    for (flag, source) in [
        (true, "flag --wait-timeout"),
        (false, "environment variable PIQUELD_WAIT_TIMEOUT"),
    ] {
        let timeout_server = start_server(false, 1, move |request| {
            assert_eq!(request.path, TIMELINE_STREAM);
            Reply::timeline(&operation("requested"))
        });
        let mut command = timeout_server.command();
        if flag {
            command.args(["--wait-timeout", "50ms"]);
        } else {
            command.env("PIQUELD_WAIT_TIMEOUT", "50ms");
        }
        let output = command
            .args(["--json", "operation", "operation-01"])
            .output()
            .expect("piquelctl process");
        assert_eq!(output.status.code(), Some(4));
        let error = support::json_error(&output);
        assert_eq!(error["code"], "timeout");
        assert_eq!(
            error["message"],
            "stopped waiting after 50ms (--wait-timeout); the operation goes on"
        );
        assert_eq!(error["details"]["timeout"], "50ms");
        assert_eq!(error["details"]["timeout_source"], source);
        let _ = timeout_server.finish();
    }

    let interrupt_server = start_server(false, usize::MAX, move |request| {
        assert_eq!(request.path, TIMELINE_STREAM);
        // Keep the request pending when SIGINT arrives, rather than only
        // testing interruption during the sleep between completed requests.
        thread::sleep(Duration::from_millis(300));
        Reply::dropped()
    });
    let mut output = None;
    for _ in 0..3 {
        // Rebuild the command each attempt; `Command::args` accumulates, so
        // reusing one command would append duplicate arguments.
        let child = interrupt_server
            .command()
            .args(["--timeout", "5s", "operation", "operation-01"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("piquelctl child");
        thread::sleep(Duration::from_millis(150));
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("send SIGINT");
        let attempt = child.wait_with_output().expect("interrupted child");
        // A signal death before the handler was installed is a startup race;
        // retry instead of failing the test.
        if attempt.status.code() == Some(130) {
            output = Some(attempt);
            break;
        }
    }
    let output = output.expect("interrupted run reported an exit code");
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("was not cancelled"));
    let _ = interrupt_server.stop();
}
