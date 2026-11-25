//! Backup repositories.

use super::*;

/// `backup repository create` sends the path as a local location and prints
/// the recovery key even with `--quiet`, since it is shown only once; `list`
/// says what the latest check found.
#[test]
fn backup_repositories_print_their_recovery_key_once_and_their_checks() {
    let repository = |check: Value| {
        json!({
            "name": "local",
            "location": {"kind": "local", "path": "/srv/backups"},
            "created_at_ms": 1000,
            "last_check": check,
        })
    };
    let created = repository(Value::Null);
    let failed = repository(json!({"checked_at_ms": 2000, "problem": "pack 1f is missing"}));
    let server = start_server(false, 2, move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/api/v1/backup/repositories/local") => {
                let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
                assert_eq!(
                    body,
                    json!({"location": {"kind": "local", "path": "/srv/backups"}})
                );
                Reply::json(json!({"repository": created.clone(), "recovery_key": "a1b2c3"}))
            }
            ("GET", "/api/v1/backup/repositories") => Reply::json(json!([failed.clone()])),
            (method, path) => panic!("unexpected {method} {path}"),
        }
    });
    let output = run_human(
        &server,
        &[
            "--quiet",
            "backup",
            "repository",
            "create",
            "local",
            "--path",
            "/srv/backups",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a1b2c3"), "{stdout}");
    let output = run_human(&server, &["backup", "repository", "list"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("local /srv/backups"), "{stdout}");
    assert!(stdout.contains("pack 1f is missing"), "{stdout}");
    let _ = server.finish();
}

/// A relative path is refused before anything is sent.
#[test]
fn backup_repository_paths_must_be_absolute() {
    let server = start_server(false, usize::MAX, |request| {
        panic!("unexpected {} {}", request.method, request.path)
    });
    let output = run(
        &server,
        &[
            "backup",
            "repository",
            "create",
            "local",
            "--path",
            "backups",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let _ = server.finish();
}
