//! Build listings and build logs.

use super::*;

#[test]
fn builds_list_resolves_names_and_sends_its_filters() {
    for (arguments, filter) in [
        (
            &["builds", "list", "--application", "app-notes-01"][..],
            "application_id=app-notes-01&cursor=v1%3A7&limit=50",
        ),
        (
            &["build", "list", "--app", "notes", "--limit", "5"],
            "application_id=app-notes-01&cursor=v1%3A7&limit=5",
        ),
        (
            &["build", "list", "--app", "welcome-to-docker"],
            "application_id=app-welcome-01&cursor=v1%3A7&limit=50",
        ),
        (
            &["build", "list", "--app", "notes", "--env", "production"],
            "application_id=app-notes-01&cursor=v1%3A7&environment_id=app-notes-01&limit=50",
        ),
        (
            &["build", "list", "--app", "notes", "--env", "env-preview-01"],
            "application_id=app-notes-01&cursor=v1%3A7&environment_id=env-preview-01&limit=50",
        ),
    ] {
        let server = start_server(false, usize::MAX, |request| match request.path.as_str() {
            "/api/v1/applications?name=notes" => {
                Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
            }
            "/api/v1/applications?name=welcome-to-docker" => Reply::json(page(
                vec![app_summary("app-welcome-01", "welcome-to-docker")],
                None,
            )),
            "/api/v1/applications/app-notes-01" => Reply::json(notes_with_a_preview()),
            "/api/v1/applications/app-welcome-01" => {
                Reply::json(app_view("app-welcome-01", "welcome-to-docker"))
            }
            "/api/v1/applications/welcome-to-docker" => Reply::status(
                "404 Not Found",
                &json!({"code": "not_found", "message": "not found"}),
            ),
            path if path.starts_with("/api/v1/builds") => Reply::json(page(Vec::new(), None)),
            path => panic!("unexpected path {path}"),
        });
        assert_json_success(&run(&server, &[arguments, &["--cursor", "v1:7"]].concat()));
        let requests = server.finish();
        let last = requests.last().expect("a builds request");
        assert_eq!(
            last.path,
            format!("/api/v1/builds?{filter}"),
            "{arguments:?}"
        );
    }
    for arguments in [
        &["--app", "blog"][..],
        &["--app", "app-gone-01"],
        &["--app", "notes", "--env", "missing"],
    ] {
        let server = start_server(false, usize::MAX, |request| match request.path.as_str() {
            "/api/v1/applications?name=notes" => {
                Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
            }
            "/api/v1/applications/app-notes-01" => Reply::json(notes_with_a_preview()),
            "/api/v1/applications?name=blog" | "/api/v1/applications?name=app-gone-01" => {
                Reply::json(page(Vec::new(), None))
            }
            "/api/v1/applications/app-gone-01" => Reply::status(
                "404 Not Found",
                &json!({"code": "not_found", "message": "not found"}),
            ),
            path => panic!("unexpected path {path}"),
        });
        let output = run(&server, &[&["build", "list"][..], arguments].concat());
        let requests = server.finish();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            !requests
                .iter()
                .any(|request| request.path.starts_with("/api/v1/builds")),
            "{arguments:?}"
        );
    }
}

#[test]
fn build_logs_render_structured_output_and_page_backwards() {
    for before in [None, Some("128")] {
        let server = start_server(false, 1, move |request| {
            let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
            assert_eq!(path, "/api/v1/builds/1/logs");
            assert_eq!(
                query.trim_end_matches('&'),
                before.map_or(String::new(), |value| format!("before={value}"))
            );
            Reply::json(json!({
                "items": [{"offset":64,"timestamp_ms":1000,"stream":"stderr","text":"\u{1b}[31mERROR failed\u{1b}[0m\n"}],
                "previous_offset":64,"truncated":false,"expired":false
            }))
        });
        let mut args = vec!["builds", "logs", "1"];
        if let Some(before) = before {
            args.extend(["--before", before]);
        }
        let output = run_human(&server, &args);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("the bytes are utf-8"),
            "ERROR failed\n"
        );
        assert!(
            String::from_utf8(output.stderr)
                .expect("the bytes are utf-8")
                .contains("--before 64")
        );
        server.finish();
    }
}
