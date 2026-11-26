//! Notification receivers and their deliveries.

use super::*;

/// `notification set --url-stdin` refuses a URL longer than any webhook's
/// rather than cutting it, also when whitespace or blank lines pad it past the
/// read limit.
#[test]
fn notification_urls_over_8_kib_are_refused() {
    let prefix = "https://discord.com/api/webhooks/";
    let exact = format!("{prefix}{}", "a".repeat(8192 - prefix.len()));
    let short = &exact[1..];
    for input in [
        format!("{prefix}{}", "a".repeat(8192)),
        format!("    {exact}"),
        format!("{short}\r\n\r\nmore"),
    ] {
        let server = start_server(false, 1, |request| match request.path.as_str() {
            "/api/v1/notifications/destinations" => Reply::json(json!([])),
            path => panic!("unexpected path {path}"),
        });
        let mut child = server
            .command()
            .args([
                "--json",
                "notification",
                "set",
                "ops",
                "--kind",
                "discord",
                "--url-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("piquelctl process");
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(input.as_bytes())
            .expect("URL is written");
        let output = child.wait_with_output().expect("piquelctl exits");
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            support::json_error(&output)["message"]
                .as_str()
                .expect("the value is a string")
                .contains("at most 8192 bytes")
        );
        let _ = server.finish();
    }
}

/// `notification deliveries` reads the requested page, showing when each
/// delivery changed or, while pending, is next attempted; `retry` fails with
/// the reason the daemon refused it.
#[test]
fn notification_deliveries_page_and_retry_refusals() {
    let delivery = |id: &str, state: &str, attempts: i64, error: Option<&str>| {
        json!({
            "id": id, "event_id": 7, "destination": "ops",
            "subject": {"application": "notes", "environment": "production"},
            "category": "deployment_failures", "state": state, "attempts": attempts,
            "created_at_ms": 1000, "next_attempt_ms": 3000, "updated_at_ms": 2000,
            "last_error": error,
        })
    };
    let refusal = json!({
        "code": "delivery_not_retryable",
        "message": "the alert's incident has closed, so it cannot be retried",
        "request_id": "request-01",
    });
    let server = start_server(false, 2, move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v1/notifications/deliveries?cursor=v1%3Adelivery-03&limit=2") => {
                Reply::json(page(
                    vec![
                        delivery("delivery-02", "failed", 1, Some("HTTP 404")),
                        delivery("delivery-01", "pending", 2, None),
                    ],
                    Some("v1:delivery-01"),
                ))
            }
            ("POST", "/api/v1/notifications/deliveries/delivery-02/retry") => {
                Reply::status("409 Conflict", &refusal)
            }
            request => panic!("unexpected request {request:?}"),
        }
    });
    let arguments = ["--cursor", "v1:delivery-03", "--limit", "2"];
    let output = run_human(
        &server,
        &[&["notification", "deliveries"], &arguments[..]].concat(),
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("the bytes are utf-8"),
        "delivery-02  ops  deployment_failures  notes / production  failed  1 attempt  updated 1970-01-01 00:00:02Z  HTTP 404\n\
         delivery-01  ops  deployment_failures  notes / production  pending  2 attempts  next attempt 1970-01-01 00:00:03Z\n\
         Next cursor: v1:delivery-01\n"
    );
    let output = run_human(&server, &["notification", "retry", "delivery-02"]);
    server.finish();
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).expect("the bytes are utf-8");
    assert!(
        stderr.contains("the alert's incident has closed"),
        "{stderr}"
    );
}
