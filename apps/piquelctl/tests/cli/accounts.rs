//! Logins, credentials, accounts and the audit log.

use super::*;

#[test]
fn rejected_credentials_exit_7_with_a_hint_and_throttling_exits_4() {
    for (status, code, exit, hint) in [
        (
            "401 Unauthorized",
            "authentication_required",
            7,
            "piquelctl login",
        ),
        (
            "403 Forbidden",
            "permission_denied",
            7,
            "ask an account owner",
        ),
        ("408 Request Timeout", "request_timeout", 4, ""),
        ("429 Too Many Requests", "rate_limited", 4, ""),
    ] {
        let server = start_server(false, 1, move |_| {
            Reply::status(status, &json!({"code": code, "message": "refused"}))
        });
        let output = run_human(&server, &["operation", "operation-01", "--no-wait"]);
        assert_eq!(output.status.code(), Some(exit), "{status}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(hint), "{status}: {stderr}");
        assert_eq!(
            stderr.contains("Credential"),
            exit == 7,
            "{status}: {stderr}"
        );
        let _ = server.finish();
    }
}

#[test]
fn logout_without_a_credential_sends_nothing() {
    let output = support::command()
        .args(["--socket", "/nonexistent/piqueld.sock", "logout"])
        .output()
        .expect("piquelctl process");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("No saved login for this daemon"));
}

/// Daemons older than limited credentials ignore requested grants and issue
/// the account's full access, so the CLI refuses before asking them for any.
#[test]
fn limited_credentials_are_never_requested_from_older_daemons() {
    let server = start_server(false, 3, |request| {
        match (request.method.as_str(), request.path.as_str()) {
            // An older session has no `scoped` field.
            ("GET", "/api/v1/auth/me") => {
                let user = json!({"id": "alice", "username": "alice", "display_name": ""});
                Reply::bare(&json!({"user": user, "grants": [{"permission": "admin"}]}))
            }
            ("GET", "/api/v1/auth/status") => Reply::bare(&json!({
                "initialized": true,
                "public_url": "http://localhost:7845"
            })),
            // An older daemon ignores the body and echoes no grants.
            ("POST", "/api/v1/auth/device/start") => Reply::bare(&json!({
                "device_code": "device",
                "user_code": "ABCD-EFGH",
                "verification_uri": "http://localhost:7845/dashboard/auth#device",
                "expires_in": 600,
                "interval": 5,
                "requester": null
            })),
            other => panic!("unexpected request {other:?}"),
        }
    });
    let token = run(
        &server,
        &["token", "create", "ci", "--permission", "apps:read"],
    );
    assert!(!token.status.success());
    let stderr = String::from_utf8_lossy(&token.stderr);
    assert!(stderr.contains("predates limited tokens"), "{stderr}");
    let login = run(&server, &["login", "--permission", "apps:read"]);
    assert!(!login.status.success());
    let stderr = String::from_utf8_lossy(&login.stderr);
    assert!(stderr.contains("predates limited logins"), "{stderr}");
    let paths: Vec<_> = server
        .finish()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths,
        [
            "/api/v1/auth/me",
            "/api/v1/auth/status",
            "/api/v1/auth/device/start"
        ]
    );
}

/// Auditors without accounts:manage only see themselves in the directory, so
/// other accounts are matched by ID when given one, and otherwise by the
/// username their requests recorded.
#[test]
fn audit_matches_accounts_missing_from_the_directory() {
    let deleted = "0199b0a4-7c1e-7d2a-9f3e-2b6c8d4e5f60";
    let server = start_server(false, 4, |request| match request.path.as_str() {
        "/api/v1/auth/directory" => Reply::bare(&json!({
            "users": [{
                "user": {"id": "auditor-id", "username": "auditor", "display_name": ""},
                "grants": [{"permission": "audit:read"}]
            }],
            "passkeys": [],
            "credentials": [],
            "invitations": []
        })),
        path if path.starts_with("/api/v1/audit?") => Reply::json(page(Vec::new(), None)),
        path => panic!("unexpected path {path}"),
    });
    for user in ["alice", deleted] {
        assert_json_success(&run(&server, &["audit", "--user", user]));
    }
    let audits: Vec<_> = server
        .finish()
        .into_iter()
        .map(|request| request.path)
        .filter(|path| path.starts_with("/api/v1/audit?"))
        .collect();
    assert!(audits[0].contains("username=alice"), "{audits:?}");
    assert!(!audits[0].contains("user_id"), "{audits:?}");
    assert!(
        audits[1].contains(&format!("user_id={deleted}")),
        "{audits:?}"
    );
}

/// Grants name their applications from every page of the application list,
/// not only the first.
#[test]
fn whoami_names_grant_applications_past_the_first_page() {
    let server = start_server(false, 3, |request| match request.path.as_str() {
        "/api/v1/auth/me" => Reply::bare(&json!({
            "user": {"id": "bob-id", "username": "bob", "display_name": "Bob"},
            "grants": [{"permission": "apps:deploy", "applications": ["app-second-01"]}],
            "scoped": false
        })),
        path if path.starts_with("/api/v1/applications") && path.contains("cursor=") => {
            Reply::json(page(vec![app_summary("app-second-01", "second")], None))
        }
        path if path.starts_with("/api/v1/applications") => Reply::json(page(
            vec![app_summary("app-first-01", "first")],
            Some("v1:app-first-01"),
        )),
        path => panic!("unexpected path {path}"),
    });
    let output = run_human(&server, &["whoami"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("apps:deploy on second"), "{stdout}");
    let _ = server.finish();
}

/// Account commands name accounts by username and send the daemon's IDs;
/// `update` keeps the profile fields it was not asked to change, and renames
/// the account's saved login so `--account` still finds it. Passkey IDs are
/// base64url credential IDs, so some start with `-` and must still parse as
/// the ID rather than as a flag. Revocations cascade unless `--no-cascade`.
#[test]
fn account_changes_resolve_usernames_to_typed_requests() {
    let reply = |request: Request| match request.path.as_str() {
        "/api/v1/auth/me" => Reply::bare(&json!({
            "user": {"id": "bob-id", "username": "bob", "display_name": "Bob"},
            "grants": [],
            "scoped": false
        })),
        "/api/v1/auth/directory" => Reply::bare(&json!({
            "users": [{
                "user": {"id": "bob-id", "username": "bob", "display_name": "Bob"},
                "grants": []
            }],
            "passkeys": [],
            "credentials": [],
            "invitations": []
        })),
        "/api/v1/auth/manage" => Reply::bare(&json!({})),
        "/api/v1/applications?limit=100" => Reply::json(page(Vec::new(), None)),
        path => panic!("unexpected path {path}"),
    };
    // `update` syncs the renamed login to disk, which on a busy disk can
    // outlast the server's `ACCEPT_TIMEOUT`, so it gets a server of its own
    // and the other changes one started after it.
    let server = start_server(false, 2, reply);
    let Endpoint::Tcp(url) = &server.endpoint else {
        panic!("TCP server");
    };
    // Saved logins are keyed by the endpoint without its trailing slash.
    let url = url.trim_end_matches('/');
    let directory = tempdir().expect("credentials directory");
    let credentials = directory.path().join("credentials.json");
    let login = json!({"username": "bob", "token": "pqd_bob"});
    let saved = json!({"endpoints": {url: {"selected": "bob-id", "accounts": {"bob-id": login}}}});
    fs::write(&credentials, saved.to_string()).unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let mut update = server.command();
    update.env("PIQUELD_CREDENTIALS_FILE", &credentials);
    let output = update
        .args(["--json", "--account", "bob", "account", "update", "bob"])
        .args(["--username", "robert"])
        .output()
        .unwrap();
    assert_json_success(&output);
    let saved: Value = serde_json::from_slice(&fs::read(&credentials).unwrap()).unwrap();
    assert_eq!(
        saved["endpoints"][url]["accounts"]["bob-id"],
        json!({"username": "robert", "token": "pqd_bob"})
    );
    let mut requests = server.finish();
    let server = start_server(false, 12, reply);
    for arguments in [
        &["account", "delete", "bob", "--yes"][..],
        &["account", "passkey", "rename", "-_key", "laptop"],
        &[
            "account",
            "passkey",
            "remove",
            "-_key",
            "--yes",
            "--no-cascade",
        ],
        &["account", "invitation", "revoke", "inv-1"],
        &["token", "revoke", "--all", "--user", "bob", "--yes"],
        &["token", "revoke", "--all", "--yes", "--no-cascade"],
        &["token", "revoke", "token-1"],
    ] {
        let output = run(&server, arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    requests.extend(server.finish());
    let sent: Vec<Value> = requests
        .into_iter()
        .filter(|request| request.path == "/api/v1/auth/manage")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(
        sent,
        [
            json!({"action": "update_user", "user_id": "bob-id", "username": "robert", "display_name": "Bob"}),
            json!({"action": "delete_user", "user_id": "bob-id", "cascade": true}),
            json!({"action": "rename_passkey", "id": "-_key", "name": "laptop"}),
            json!({"action": "remove_passkey", "id": "-_key", "cascade": false}),
            json!({"action": "revoke_invitation", "id": "inv-1"}),
            json!({"action": "revoke_all", "user_id": "bob-id", "cascade": true}),
            json!({"action": "revoke_all", "user_id": "bob-id", "cascade": false}),
            json!({"action": "revoke_credential", "id": "token-1", "cascade": true}),
        ]
    );
}
