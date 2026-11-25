use super::*;
use crate::dns::provider::{DnsProvider, tests::Recorded};

fn ovh(api: String) -> Ovh {
    Ovh {
        endpoint: OvhEndpoint::OvhEu,
        application_key: "ak".into(),
        application_secret: "as".into(),
        consumer_key: "ck".into(),
        api,
        clock: Clock::default(),
    }
}

/// The expected signature was computed independently: `printf
/// 'as+ck+GET+https://eu.api.ovh.com/1.0/domain/zone++1700000000' | sha1sum`.
#[test]
fn signature_covers_method_url_body_and_time() {
    assert_eq!(
        ovh(OvhEndpoint::OvhEu.url().into()).signature(
            &Method::GET,
            "https://eu.api.ovh.com/1.0/domain/zone",
            "",
            1_700_000_000
        ),
        "$1$a83d7f6455abba1184731d8daa59ad88cc891a0a"
    );
}

#[tokio::test]
async fn records_are_listed_by_name_and_cnames_are_absolute() {
    let recorded = Recorded::serve(vec![
        (Method::GET, "/auth/time", 200, "1700000000"),
        (Method::GET, "/domain/zone/piquel.fr/record?subDomain=www", 200, "[1,2,3]"),
        (Method::GET, "/domain/zone/piquel.fr/record/1", 200,
            r#"{"id":1,"zone":"piquel.fr","fieldType":"CNAME","subDomain":"www","target":"web.example.com.","ttl":300}"#),
        (Method::GET, "/domain/zone/piquel.fr/record/2", 200,
            r#"{"id":2,"zone":"piquel.fr","fieldType":"TXT","subDomain":"www","target":"\"owner\"","ttl":60}"#),
        (Method::GET, "/domain/zone/piquel.fr/record/3", 200,
            r#"{"id":3,"zone":"piquel.fr","fieldType":"A","subDomain":"api","target":"192.0.2.1","ttl":300}"#),
        (Method::PUT, "/domain/zone/piquel.fr/record/1", 200, "null"),
        (Method::POST, "/domain/zone/piquel.fr/record", 200,
            r#"{"id":4,"zone":"piquel.fr","fieldType":"A","subDomain":"www","target":"192.0.2.9","ttl":300}"#),
    ]).await;
    let provider = DnsProvider::Ovh(ovh(recorded.url.clone()));
    let http = reqwest::Client::new();
    let zone = Zone {
        name: Hostname::parse("piquel.fr").unwrap(),
        id: "piquel.fr".into(),
    };
    let found = provider
        .records(&http, &zone, "www.piquel.fr")
        .await
        .unwrap();
    let target = Hostname::parse("web2.example.com").unwrap();
    assert_eq!(
        found.iter().map(|f| f.record.clone()).collect::<Vec<_>>(),
        [
            Record::Cname {
                target: Hostname::parse("web.example.com").unwrap(),
                proxied: false
            },
            Record::Txt("owner".into()),
        ]
    );
    let cname = Record::Cname {
        target: target.clone(),
        proxied: false,
    };
    provider
        .upsert(&http, &zone, "www.piquel.fr", Some(&found[0].id), &cname)
        .await
        .unwrap();
    let id = provider
        .upsert(
            &http,
            &zone,
            "www.piquel.fr",
            None,
            &Record::A("192.0.2.9".parse().unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(id, RecordId("4".into()));
    let error = provider
        .upsert(
            &http,
            &zone,
            "www.piquel.fr",
            None,
            &Record::Cname {
                target,
                proxied: true,
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{:#}", anyhow::Error::from(error)).contains("OVH cannot proxy"));
    let signed: Vec<_> = recorded
        .requests()
        .await
        .into_iter()
        .filter(|r| r.path != "/auth/time")
        .collect();
    assert_eq!(signed.len(), 6);
    assert_eq!(
        signed[4].body,
        json!({"subDomain":"www","target":"web2.example.com.","ttl":300})
    );
    assert_eq!(
        signed[5].body,
        json!({"fieldType":"A","subDomain":"www","target":"192.0.2.9","ttl":300})
    );
}

/// The apex is listed one record type at a time, not as the whole zone,
/// and the API's clock is read once for every call.
#[tokio::test]
async fn apex_records_are_listed_by_type_with_one_clock_read() {
    let recorded = Recorded::serve(vec![
        (Method::GET, "/auth/time", 200, "1700000000"),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record?subDomain=&fieldType=A",
            200,
            "[1]",
        ),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record?subDomain=&fieldType=AAAA",
            200,
            "[]",
        ),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record?subDomain=&fieldType=CNAME",
            200,
            "[]",
        ),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record?subDomain=&fieldType=TXT",
            200,
            "[2]",
        ),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record/1",
            200,
            r#"{"id":1,"fieldType":"A","subDomain":"","target":"192.0.2.1"}"#,
        ),
        (
            Method::GET,
            "/domain/zone/piquel.fr/record/2",
            200,
            r#"{"id":2,"fieldType":"TXT","subDomain":"www","target":"other"}"#,
        ),
    ])
    .await;
    let provider = DnsProvider::Ovh(ovh(recorded.url.clone()));
    let zone = Zone::named(Hostname::parse("piquel.fr").unwrap());
    let found = provider
        .records(&reqwest::Client::new(), &zone, "piquel.fr")
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|f| f.record.clone()).collect::<Vec<_>>(),
        [Record::A("192.0.2.1".parse().unwrap())]
    );
    let requests = recorded.requests().await;
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests.iter().filter(|r| r.path == "/auth/time").count(),
        1
    );
}

#[tokio::test]
async fn records_are_relative_to_their_zone_and_published_by_refresh() {
    let recorded = Recorded::serve(vec![
        (Method::GET, "/auth/time", 200, "1700000000"),
        (Method::GET, "/domain/zone", 200, r#"["piquel.fr","bad_zone"]"#),
        (Method::POST, "/domain/zone/piquel.fr/record", 200,
            r#"{"id":5170142,"zone":"piquel.fr","fieldType":"TXT","subDomain":"_acme-challenge.staging","target":"v","ttl":60}"#),
        (Method::POST, "/domain/zone/piquel.fr/refresh", 200, ""),
        (Method::DELETE, "/domain/zone/piquel.fr/record/5170142", 200, "null"),
        (Method::DELETE, "/domain/zone/piquel.fr/record/1", 404,
            r#"{"message":"The requested object (id = 1) does not exist"}"#),
    ]).await;
    let provider = DnsProvider::Ovh(ovh(recorded.url.clone()));
    let http = reqwest::Client::new();
    let zones = provider.zones(&http).await.unwrap();
    assert_eq!(zones.len(), 1);
    let id = provider
        .upsert(
            &http,
            &zones[0],
            "_acme-challenge.staging.piquel.fr",
            None,
            &Record::Txt("v".into()),
        )
        .await
        .unwrap();
    assert_eq!(id, RecordId("5170142".into()));
    provider.publish(&http, &zones[0]).await.unwrap();
    provider.delete_record(&http, &zones[0], &id).await.unwrap();
    provider.publish(&http, &zones[0]).await.unwrap();
    provider
        .delete_record(&http, &zones[0], &RecordId("1".into()))
        .await
        .unwrap();
    let requests = recorded.requests().await;
    let signed: Vec<_> = requests.iter().filter(|r| r.path != "/auth/time").collect();
    assert_eq!(
        signed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/domain/zone",
            "/domain/zone/piquel.fr/record",
            "/domain/zone/piquel.fr/refresh",
            "/domain/zone/piquel.fr/record/5170142",
            "/domain/zone/piquel.fr/refresh",
            "/domain/zone/piquel.fr/record/1",
        ]
    );
    for request in &signed {
        assert_eq!(request.header("x-ovh-application"), Some("ak"));
        assert_eq!(request.header("x-ovh-consumer"), Some("ck"));
        assert_eq!(request.header("x-ovh-timestamp"), Some("1700000000"));
        let url = format!("{}{}", recorded.url, request.path);
        let body = if request.body.is_null() {
            String::new()
        } else {
            request.body.to_string()
        };
        let expected =
            ovh(recorded.url.clone()).signature(&request.method, &url, &body, 1_700_000_000);
        assert_eq!(request.header("x-ovh-signature"), Some(expected.as_str()));
    }
    assert_eq!(
        signed[1].body,
        json!({"fieldType":"TXT","subDomain":"_acme-challenge.staging","target":"v","ttl":60})
    );
}
