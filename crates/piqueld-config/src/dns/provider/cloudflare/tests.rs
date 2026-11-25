use super::*;
use crate::dns::provider::{DnsProvider, tests::Recorded};
use axum::http::Method;

fn provider(api: String) -> DnsProvider {
    DnsProvider::Cloudflare(Cloudflare {
        api_token: "token".into(),
        api,
    })
}

#[tokio::test]
async fn zones_follow_pages_and_records_round_trip() {
    let recorded = Recorded::serve(vec![
        (Method::GET, "/zones?per_page=50&page=1", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"},{"id":"z2","name":"local"}],"result_info":{"page":1,"per_page":50,"total_pages":2}}"#),
        (Method::GET, "/zones?per_page=50&page=2", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"z3","name":"example.com"}],"result_info":{"page":2,"per_page":50,"total_pages":2}}"#),
        (Method::POST, "/zones/z1/dns_records", 200,
            r#"{"success":true,"errors":[],"result":{"id":"r1","type":"TXT","name":"_acme-challenge.piquel.fr","content":"\"v\""}}"#),
        (Method::DELETE, "/zones/z1/dns_records/r1", 200,
            r#"{"success":true,"errors":[],"result":{"id":"r1"}}"#),
    ]).await;
    let provider = provider(recorded.url.clone());
    let http = reqwest::Client::new();
    let zones = provider.zones(&http).await.unwrap();
    assert_eq!(
        zones
            .iter()
            .map(|z| (z.name.as_str(), z.id.as_str()))
            .collect::<Vec<_>>(),
        [("piquel.fr", "z1"), ("example.com", "z3")]
    );
    let id = provider
        .upsert(
            &http,
            &zones[0],
            "_acme-challenge.piquel.fr",
            None,
            &Record::Txt("v".into()),
        )
        .await
        .unwrap();
    assert_eq!(id, RecordId("r1".into()));
    provider.delete_record(&http, &zones[0], &id).await.unwrap();
    let requests = recorded.requests().await;
    assert!(
        requests
            .iter()
            .all(|r| r.header("authorization") == Some("Bearer token"))
    );
    assert_eq!(
        requests[2].body,
        json!({"type":"TXT","name":"_acme-challenge.piquel.fr","content":"\"v\"","ttl":60})
    );
}

#[tokio::test]
async fn records_follow_pages_and_upserts_create_or_replace() {
    const LIST: &str = "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100";
    let recorded = Recorded::serve(vec![
        (Method::GET, "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100&page=1", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"r1","type":"A","name":"www.piquel.fr","content":"192.0.2.1","proxied":false},{"id":"r2","type":"MX","name":"www.piquel.fr","content":"mail.piquel.fr"}],"result_info":{"page":1,"per_page":100,"total_pages":2}}"#),
        (Method::GET, "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100&page=2", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"r3","type":"CNAME","name":"www.piquel.fr","content":"t.cfargotunnel.com","proxied":true},{"id":"r4","type":"TXT","name":"www.piquel.fr","content":"\"v=spf1 -all\""}],"result_info":{"page":2,"per_page":100,"total_pages":2}}"#),
        (Method::PUT, "/zones/z1/dns_records/r1", 200,
            r#"{"success":true,"errors":[],"result":{"id":"r1"}}"#),
        (Method::POST, "/zones/z1/dns_records", 200,
            r#"{"success":true,"errors":[],"result":{"id":"r5"}}"#),
    ]).await;
    let provider = provider(recorded.url.clone());
    let http = reqwest::Client::new();
    let zone = Zone {
        name: Hostname::parse("piquel.fr").unwrap(),
        id: "z1".into(),
    };
    let found = provider
        .records(&http, &zone, "www.piquel.fr")
        .await
        .unwrap();
    let tunnel = Hostname::parse("t.cfargotunnel.com").unwrap();
    assert_eq!(
        found
            .iter()
            .map(|f| (f.id.0.as_str(), f.record.clone()))
            .collect::<Vec<_>>(),
        [
            ("r1", Record::A("192.0.2.1".parse().unwrap())),
            (
                "r3",
                Record::Cname {
                    target: tunnel.clone(),
                    proxied: true
                }
            ),
            ("r4", Record::Txt("v=spf1 -all".into())),
        ]
    );
    let replaced = provider
        .upsert(
            &http,
            &zone,
            "www.piquel.fr",
            Some(&found[0].id),
            &Record::Aaaa("2001:db8::1".parse().unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(replaced, RecordId("r1".into()));
    let created = provider
        .upsert(
            &http,
            &zone,
            "www.piquel.fr",
            None,
            &Record::Cname {
                target: tunnel,
                proxied: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(created, RecordId("r5".into()));
    let requests = recorded.requests().await;
    assert!(requests[0].path.starts_with(LIST));
    assert_eq!(
        requests[2].body,
        json!({"type":"AAAA","name":"www.piquel.fr","content":"2001:db8::1","ttl":300,"proxied":false})
    );
    // Proxied records use Cloudflare's automatic TTL.
    assert_eq!(
        requests[3].body,
        json!({"type":"CNAME","name":"www.piquel.fr","content":"t.cfargotunnel.com","ttl":1,"proxied":true})
    );
}

#[tokio::test]
async fn discovery_is_reused_for_an_hour_and_failures_back_off() {
    const ZONES: &str = "/zones?per_page=50&page=1";
    let working = Recorded::serve(vec![(
        Method::GET,
        ZONES,
        200,
        r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"}]}"#,
    )])
    .await;
    let failing = Recorded::serve(vec![(Method::GET, ZONES, 500, "{}")]).await;
    let mut dns = crate::dns::Dns::new(vec![
        provider(working.url.clone()).into(),
        provider(failing.url.clone()).into(),
    ])
    .unwrap();
    let calls = async || {
        (
            working.requests().await.len(),
            failing.requests().await.len(),
        )
    };
    // A failing provider waits out its backoff instead of making every
    // refresh rediscover every provider.
    dns.refresh().await;
    dns.refresh().await;
    assert_eq!(calls().await, (1, 1));
    dns.discover().await;
    assert_eq!(calls().await, (1, 1));
    // A failed discovery keeps the zones.
    dns.providers[0] = provider(failing.url.clone()).into();
    dns.discover().await;
    let status = &dns.status().await[0];
    assert!(
        !status.healthy && status.zones == ["piquel.fr"],
        "{status:?}"
    );
    dns.providers[0] = provider(working.url.clone()).into();
    dns.refresh().await;
    assert_eq!(calls().await, (0, 2));
    // Once its backoff has elapsed, the next refresh retries it alone.
    dns.discovered.write().await[0].due = Some(std::time::Instant::now());
    dns.refresh().await;
    assert_eq!(calls().await, (1, 0));
    assert!(dns.status().await[0].healthy);
}

#[tokio::test]
async fn lists_stop_at_unrequested_pages_and_large_bodies() {
    let large: &'static str = format!("[{}]", "0,".repeat(2 << 20) + "0").leak();
    let recorded = Recorded::serve(vec![
        // A server that ignores `page` answers page 1 again.
        (Method::GET, "/zones?per_page=50&page=1", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"}],"result_info":{"page":1,"per_page":50,"total_pages":2}}"#),
        (Method::GET, "/zones?per_page=50&page=2", 200,
            r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"}],"result_info":{"page":1,"per_page":50,"total_pages":2}}"#),
        (Method::GET, "/zones/z1/dns_records?name.exact=piquel.fr&per_page=100&page=1", 200, large),
    ]).await;
    let provider = provider(recorded.url.clone());
    let http = reqwest::Client::new();
    let error = provider.zones(&http).await.unwrap_err();
    assert!(error.to_string().contains("list zones"), "{error}");
    assert!(matches!(
        std::error::Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(ApiError::Pagination(2))
    ));
    let zone = Zone {
        name: Hostname::parse("piquel.fr").unwrap(),
        id: "z1".into(),
    };
    let error = provider
        .records(&http, &zone, "piquel.fr")
        .await
        .unwrap_err();
    assert!(matches!(
        std::error::Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(ApiError::TooLarge(_))
    ));
}

/// A first page answered as the last one would otherwise pass for the
/// whole list, and replace the discovered zones with part of them.
#[tokio::test]
async fn a_last_page_that_was_not_asked_for_fails() {
    let recorded = Recorded::serve(vec![(Method::GET, "/zones?per_page=50&page=1", 200,
        r#"{"success":true,"errors":[],"result":[{"id":"z2","name":"example.com"}],"result_info":{"page":2,"per_page":50,"total_pages":2}}"#)]).await;
    let error = provider(recorded.url.clone())
        .zones(&reqwest::Client::new())
        .await
        .unwrap_err();
    assert!(matches!(
        std::error::Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(ApiError::Pagination(1))
    ));
}

#[tokio::test]
async fn api_errors_name_the_provider_and_zone_but_not_the_token() {
    let recorded = Recorded::serve(vec![(Method::POST, "/zones/z1/dns_records", 403,
        r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"result":null}"#)]).await;
    let zone = Zone {
        name: Hostname::parse("piquel.fr").unwrap(),
        id: "z1".into(),
    };
    let error = provider(recorded.url.clone())
        .upsert(
            &reqwest::Client::new(),
            &zone,
            "_acme-challenge.piquel.fr",
            None,
            &Record::Txt("v".into()),
        )
        .await
        .unwrap_err();
    let message = format!("{:#}", anyhow::Error::from(error));
    assert!(
        message.starts_with("cloudflare write record in zone piquel.fr: HTTP 403"),
        "{message}"
    );
    assert!(message.contains("Authentication error"), "{message}");
    assert!(!message.contains("token"), "{message}");
}
