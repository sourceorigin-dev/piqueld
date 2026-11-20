//! Durable audit: which requests record their changes in the changes' own
//! transaction, and that each converted family does so once.
use super::*;

/// Write routes that change no persistent state, such as a plan, a test
/// send or an in-memory sign-in step, so they are recorded in the
/// background.
const BACKGROUND: [(&str, &str); 12] = [
    ("POST", "/api/v1/applications/plan"),
    ("POST", "/api/v1/environments/{id}/promote/plan"),
    ("POST", "/api/v1/auth/device/start"),
    ("POST", "/api/v1/auth/device/approve"),
    ("POST", "/api/v1/auth/device/confirm"),
    ("POST", "/api/v1/auth/device/inspect"),
    ("POST", "/api/v1/auth/login/start"),
    ("POST", "/api/v1/auth/register/start"),
    ("POST", "/api/v1/auth/reauthenticate/start"),
    ("POST", "/api/v1/notifications/destinations/{name}/test"),
    ("POST", "/api/v1/system/builder/check"),
    ("POST", "/api/v1/system/dns/refresh"),
];

/// Write routes outside the typed application edits whose changes append
/// their record in the transaction committing them: acceptance, secrets,
/// notifications, webhooks, accounts and sign-ins.
const COMMITTED: [(&str, &str); 42] = [
    ("POST", "/api/v1/applications"),
    ("POST", "/api/v1/applications/apply"),
    ("DELETE", "/api/v1/applications/{id}"),
    ("POST", "/api/v1/applications/{id}/environments"),
    ("POST", "/api/v1/applications/{id}/previews"),
    ("POST", "/api/v1/applications/{id}/previews/local"),
    ("POST", "/api/v1/applications/{id}/previews/prune"),
    ("POST", "/api/v1/applications/{id}/rename"),
    ("PUT", "/api/v1/applications/{id}/secrets/{name}"),
    ("DELETE", "/api/v1/applications/{id}/secrets/{name}"),
    ("PUT", "/api/v1/applications/{id}/secrets/{name}/access"),
    ("POST", "/api/v1/applications/{id}/webhook/secret"),
    ("DELETE", "/api/v1/environments/{id}"),
    ("PUT", "/api/v1/environments/{id}/branch"),
    ("POST", "/api/v1/environments/{id}/deploy"),
    ("POST", "/api/v1/environments/{id}/promote"),
    ("POST", "/api/v1/environments/{id}/reconcile"),
    ("POST", "/api/v1/environments/{id}/rename"),
    ("POST", "/api/v1/environments/{id}/restore"),
    ("DELETE", "/api/v1/environments/{id}/secrets/{name}"),
    (
        "POST",
        "/api/v1/environments/{id}/secrets/{name}/regenerate",
    ),
    ("PUT", "/api/v1/environments/{id}/source"),
    ("POST", "/api/v1/environments/{id}/start"),
    ("POST", "/api/v1/environments/{id}/stop"),
    ("PUT", "/api/v1/environments/{id}/sync"),
    ("PUT", "/api/v1/environments/{id}/telemetry"),
    ("DELETE", "/api/v1/previews/{id}"),
    ("POST", "/api/v1/previews/{id}/deploy"),
    ("POST", "/api/v1/previews/{id}/restore"),
    ("POST", "/api/v1/notifications/deliveries/{id}/retry"),
    ("PUT", "/api/v1/notifications/destinations/{name}"),
    ("DELETE", "/api/v1/notifications/destinations/{name}"),
    ("POST", "/api/v1/system/secrets/recover-key"),
    ("POST", "/api/v1/auth/manage"),
    ("POST", "/api/v1/auth/logout"),
    ("POST", "/api/v1/auth/login/finish"),
    ("POST", "/api/v1/auth/login/operator"),
    ("POST", "/api/v1/auth/reauthenticate/finish"),
    ("POST", "/api/v1/auth/register/finish"),
    ("POST", "/api/v1/auth/device/poll"),
    ("POST", "/api/v1/auth/recovery"),
    ("POST", "/api/v1/auth/sign-in-link"),
];

/// Resources of an application whose routes are typed edits, which all
/// submit through acceptance (`edit_endpoint!`).
const EDITED: [&str; 11] = [
    "services",
    "variables",
    "volumes",
    "routes",
    "exports",
    "imports",
    "jobs",
    "generated-secrets",
    "repository",
    "environments/{environment}/",
    "name",
];

/// Whether the write route `method` `path` is classified above.
fn classified(method: &str, path: &str) -> bool {
    let edit = path
        .strip_prefix("/api/v1/applications/{id}/")
        .is_some_and(|resource| EDITED.iter().any(|edited| resource.starts_with(edited)));
    edit || BACKGROUND.contains(&(method, path)) || COMMITTED.contains(&(method, path))
}

/// Every write route of the API is classified: its successful changes
/// record themselves in their own transaction, or it changes no persistent
/// state and is recorded in the background. A new route fails here until
/// it is classified, so none silently falls back to background records.
#[test]
fn every_write_route_records_its_changes_or_changes_nothing() {
    let document = piqueld::api::http::openapi_document();
    let paths = document["paths"]
        .as_object()
        .expect("the document lists its paths");
    let mut unclassified = Vec::new();
    for (path, item) in paths {
        let item = item.as_object().expect("each path lists its operations");
        for method in item
            .keys()
            .filter(|method| !["get", "parameters"].contains(&method.as_str()))
        {
            let method = method.to_uppercase();
            if !classified(&method, path) {
                unclassified.push(format!("{method} {path}"));
            }
        }
    }
    assert_eq!(unclassified, Vec::<String>::new());
    let listed = BACKGROUND.iter().chain(&COMMITTED);
    let missing: Vec<_> = listed
        .filter(|(method, path)| paths[*path].get(method.to_lowercase()).is_none())
        .collect();
    assert_eq!(missing, Vec::<&(&str, &str)>::new());
}

/// One route of each family that records its own changes does so once,
/// through the HTTP layer: the request's record names what it committed,
/// and the background adds no second record of its success. Deleting a
/// secret commits, and records, its reservation and its deletion.
#[tokio::test]
async fn each_family_records_its_committed_changes_once() {
    let f = GrantFixture::new().await;
    let database = database(&f.temp);
    let root = seed_account(&database, "root", &[("admin", None)]).await;
    sign_in_with_passkey(&database, "root").await;
    let leaver = seed_account(&database, "leaver", &[]).await;
    let secret = format!("/api/v1/applications/{}/secrets/stripe", f.blog);
    let upload = |generation: &str| {
        Request::put(&secret)
            .header("authorization", format!("Bearer {root}"))
            .header("content-type", "application/octet-stream")
            .header("x-expected-generation", generation)
            .body(Body::from("value"))
            .expect("the upload is a valid request")
    };
    assert_eq!(f.router.send(upload("0")).await.status, 200);
    let delete = Request::delete(&secret)
        .header("authorization", format!("Bearer {root}"))
        .header("x-expected-generation", "1")
        .body(Body::empty())
        .expect("the deletion is a valid request");
    assert_eq!(f.router.send(delete).await.status, 200);
    let destination = serde_json::json!({
        "settings": {"kind": "json", "enabled": true, "categories": {},
            "applications": "all", "previews": false},
        "url": "https://example.com/private-hook",
    });
    let token = serde_json::json!({"action": "create_token", "grants": [], "name": "ci"});
    let webhook = format!("/api/v1/applications/{}/webhook/secret", f.blog);
    for (token, method, uri, body) in [
        (
            &root,
            "PUT",
            "/api/v1/notifications/destinations/ops",
            destination,
        ),
        (&root, "POST", webhook.as_str(), serde_json::Value::Null),
        (&root, "POST", "/api/v1/auth/manage", token),
        (
            &leaver,
            "POST",
            "/api/v1/auth/logout",
            serde_json::Value::Null,
        ),
    ] {
        let (status, body) = f.router.call(token, method, uri, body).await;
        assert_eq!(status, 200, "{method} {uri}: {body}");
    }
    f.state.drain_audit(Duration::from_secs(5)).await;
    let body = f.audit(&f.auditor, "/api/v1/audit", 7).await;
    let actions: Vec<_> = body["data"]["items"]
        .as_array()
        .expect("the trail lists records")
        .iter()
        .rev()
        .map(|event| event["action"].as_str().expect("records have an action"))
        .collect();
    let starts = [
        "PUT /api/v1/applications/{id}/secrets/{name} secret \"stripe\" generation 1",
        "DELETE /api/v1/applications/{id}/secrets/{name} secret \"stripe\" deletion ",
        "DELETE /api/v1/applications/{id}/secrets/{name} secret \"stripe\" deletion ",
        "PUT /api/v1/notifications/destinations/{name} destination \"ops\" ",
        "POST /api/v1/applications/{id}/webhook/secret webhook secret generated",
        "POST /api/v1/auth/manage token ",
        "POST /api/v1/auth/logout credential \"leaver\" of account \"leaver\" revoked alone",
    ];
    assert_eq!(actions.len(), starts.len(), "{actions:#?}");
    for (action, start) in actions.iter().zip(starts) {
        assert!(
            action.starts_with(start),
            "{action} should start with {start}"
        );
        assert!(!action.contains("private-hook"), "{action}");
    }
}
