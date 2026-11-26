use super::super::sync::tests::{Fake, Fixture, NEW, OLD, Schedule};
use super::*;
use tower::ServiceExt;

/// GitHub's documented example: secret "It's a Secret to Everybody",
/// payload "Hello, World!".
const EXAMPLE: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

/// GitHub's `X-Hub-Signature-256` for `body` under `secret`.
fn sign(secret: &[u8], body: &[u8]) -> String {
    Signature::of(secret, body)
        .expect("the signature computes")
        .to_string()
}

/// Posts a push delivery to `router`.
async fn deliver(
    router: &axum::Router,
    path: &str,
    signature: Option<&str>,
    body: Vec<u8>,
) -> http::StatusCode {
    let mut request = http::Request::post(path).header("x-github-event", "push");
    if let Some(signature) = signature {
        request = request.header("x-hub-signature-256", signature);
    }
    router
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from(body))
                .expect("the request builds"),
        )
        .await
        .expect("the request is served")
        .status()
}

#[tokio::test]
async fn only_signed_deliveries_hint_sync_which_deploys_the_head_it_lists() {
    let fixture = Fixture::new("{mode='webhook'}").await;
    let production = fixture.production().await;
    let secret = fixture
        .service
        .generate_webhook_secret(piqueld_store::Actor::Daemon, &fixture.application)
        .await
        .expect("generate webhook secret succeeds")
        .secret;
    let router = crate::api::http::webhook_router(fixture.service.clone());
    let path = format!("{WEBHOOK_PATH}{}", fixture.application);
    let forged = "dddddddddddddddddddddddddddddddddddddddd";
    let body = format!(r#"{{"ref":"refs/heads/main","after":"{forged}"}}"#).into_bytes();
    let valid = sign(secret.as_bytes(), &body);

    let wrong = sign(b"another secret", &body);
    assert_eq!(
        deliver(&router, &path, Some(&wrong), body.clone()).await,
        http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        deliver(&router, &path, None, body.clone()).await,
        http::StatusCode::UNAUTHORIZED
    );
    let unknown = format!("{WEBHOOK_PATH}app-unknown-01");
    assert_eq!(
        deliver(&router, &unknown, Some(&valid), body.clone()).await,
        http::StatusCode::UNAUTHORIZED
    );
    let oversized = vec![b' '; WEBHOOK_BODY_LIMIT + 1];
    let signed = sign(secret.as_bytes(), &oversized);
    assert_eq!(
        deliver(&router, &path, Some(&signed), oversized).await,
        http::StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        deliver(&router, "/api/v1/applications", Some(&valid), body.clone()).await,
        http::StatusCode::NOT_FOUND
    );
    let lister = Fake::with(vec![("main", OLD)]);
    let mut schedule = Schedule::default();
    let start = tokio::time::Instant::now();
    fixture
        .service
        .sync_due(&lister, &mut schedule, start)
        .await;
    assert_eq!(lister.listings(), 1);
    let later = start + std::time::Duration::from_secs(3600);
    fixture
        .service
        .sync_due(&lister, &mut schedule, later)
        .await;
    assert_eq!(lister.listings(), 1);

    *lister.heads.lock() = Some(vec![("main", NEW)]);
    assert_eq!(
        deliver(&router, &path, Some(&valid), body).await,
        http::StatusCode::NO_CONTENT
    );
    fixture
        .service
        .sync_due(&lister, &mut schedule, later)
        .await;
    assert_eq!(lister.listings(), 2);
    assert_eq!(
        fixture.synced(&production).await,
        vec![(NEW.to_owned(), "sync:webhook".to_owned())]
    );
}

#[test]
fn signatures_verify_only_their_secret_and_body() {
    let secret = b"It's a Secret to Everybody";
    let signature = Signature::parse(EXAMPLE).expect("the signature parses");
    assert!(signature.verifies(secret, b"Hello, World!"));
    assert!(!signature.verifies(secret, b"Hello, World?"));
    assert!(!signature.verifies(b"another secret", b"Hello, World!"));
    for malformed in [
        "",
        "sha1=757107ea0eb2509fc211221cce984b8a37570b6d",
        &EXAMPLE[..EXAMPLE.len() - 2],
        &EXAMPLE.replace('7', "g"),
        &EXAMPLE.replacen('7', "+", 1),
    ] {
        assert!(Signature::parse(malformed).is_none(), "{malformed}");
    }
}
