use super::{Hostname, RouteTarget, ValidatedRoute};
use crate::{
    ApiErrorCode, ApplicationId, EnvironmentId, InstanceId, ResolutionSet, ResolvedSource,
    ServiceName, api::RouteStatus, compile_application, parse_toml,
};

fn manifest(routes: &str) -> String {
    format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'\n{routes}"
    )
}

#[test]
fn exact_hosts_are_canonical_and_invalid_routes_are_rejected() {
    let route = "[[spec.routes]]\nhostname='NOTES.Example.COM.'\nservice='web'\nport=3000";
    let app = parse_toml(&manifest(route))
        .unwrap()
        .normalize(ApplicationId::parse("test-app").unwrap());
    assert_eq!(app.spec().routes[0].hostname.as_str(), "notes.example.com");
    assert_eq!(
        parse_toml(&app.export_toml().unwrap())
            .unwrap()
            .normalize(app.id().clone()),
        app
    );
    for host in [
        "*.com",
        "a.*.example.com",
        "*.*.example.com",
        "*example.com",
        "*",
        "https://example.com",
        "example.com/path",
        "example.com:443",
        "localhost",
        "127.0.0.1",
        "[::1]",
        "a..com",
        "foo.local",
        "é.example.com",
    ] {
        assert!(
            parse_toml(&manifest(&route.replace("NOTES.Example.COM.", host))).is_err(),
            "{host}"
        );
    }
    parse_toml(&manifest(&route.replace("port=3000", "port=0")))
        .expect_err("`parse_toml` should be rejected");
    parse_toml(&manifest(
        &route.replace("service='web'", "service='database'"),
    ))
    .expect_err("`parse_toml` should reject a route to an unknown service");
    parse_toml(&manifest(&format!("{route}\n{route}")))
        .expect_err("`parse_toml` should be rejected");
    let without = parse_toml(&manifest(""))
        .unwrap()
        .normalize(app.id().clone());
    assert_ne!(app.spec_hash(), without.spec_hash());
}

#[test]
fn wildcards_match_one_label_below_their_parent() {
    let route = "[[spec.routes]]\nhostname='*.Tenants.Example.COM.'\nservice='web'\nport=3000";
    let app = parse_toml(&manifest(route))
        .unwrap()
        .normalize(ApplicationId::parse("test-app").unwrap());
    let hostname = &app.spec().routes[0].hostname;
    assert_eq!(hostname.as_str(), "*.tenants.example.com");
    assert_eq!(hostname.domain().as_str(), "tenants.example.com");
    let host = |name: &str| Hostname::parse(name).unwrap();
    assert!(hostname.matches(&host("alice.tenants.example.com")));
    assert!(!hostname.matches(&host("a.alice.tenants.example.com")));
    assert!(!hostname.matches(&host("tenants.example.com")));
    assert!(hostname.is_within(&host("example.com")));
    assert!(!hostname.is_within(&host("alice.tenants.example.com")));
    assert_eq!(
        parse_toml(&app.export_toml().unwrap())
            .unwrap()
            .normalize(app.id().clone()),
        app
    );
    Hostname::parse("*.example.com").expect_err("`Hostname::parse` should be rejected");
    let claimed = [host("a.tenants.example.com")];
    assert_eq!(
        hostname.served(&claimed),
        Some(host("b.tenants.example.com"))
    );
    let longest = format!("*.{}.com", vec!["a".repeat(61); 4].join("."));
    assert_eq!(longest.len(), 253);
    assert!(
        super::RouteHostname::parse(longest)
            .unwrap()
            .served(&[])
            .is_some()
    );

    let redirect = |to: &str, exact: &str| {
        parse_toml(&manifest(&format!(
            "[[spec.routes]]\nhostname='*.example.com'\nredirect={{to='{to}'}}\n{exact}"
        )))
    };
    let www = "[[spec.routes]]\nhostname='www.example.com'\nservice='web'\nport=3000";
    redirect("https://www.example.com", www).expect("`redirect` should succeed");
    redirect("https://example.com", "").expect("`redirect` should succeed");
    let errors = redirect("https://www.example.com", "").unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|e| e.code == ApiErrorCode::RouteRedirectLoop),
        "{errors:?}"
    );
}

#[test]
fn redirects_are_validated_and_keep_the_flat_wire_shape() {
    let redirect = |fields: &str| {
        parse_toml(&manifest(&format!(
            "[[spec.routes]]\nhostname='www.example.com'\n{fields}"
        )))
    };
    let app = redirect("redirect={to='https://example.com/'}")
        .unwrap()
        .normalize(ApplicationId::parse("test-app").unwrap());
    let route = &app.spec().routes[0];
    assert_eq!(
        serde_json::to_value(route).unwrap(),
        serde_json::json!({"hostname":"www.example.com","visibility":"private","redirect":{"to":"https://example.com/","status":308,"preserve_path":true}})
    );
    let RouteTarget::Redirect { redirect: target } = &route.target else {
        panic!("redirect route")
    };
    assert_eq!(target.location(), "https://example.com{http.request.uri}");
    assert_eq!(
        parse_toml(&app.export_toml().unwrap())
            .unwrap()
            .normalize(app.id().clone()),
        app
    );
    let fixed =
        redirect("redirect={to='http://example.com:8080/a',status=302,preserve_path=false}")
            .unwrap()
            .normalize(app.id().clone());
    let RouteTarget::Redirect { redirect: fixed } = &fixed.spec().routes[0].target else {
        panic!("redirect route")
    };
    assert_eq!(fixed.location(), "http://example.com:8080/a");

    for (fields, code) in [
        (
            "service='web'\nport=80\nredirect={to='https://example.com'}",
            ApiErrorCode::RouteTargetInvalid,
        ),
        ("", ApiErrorCode::RouteTargetInvalid),
        ("service='web'", ApiErrorCode::RouteTargetInvalid),
        (
            "redirect={to='https://example.com',status=200}",
            ApiErrorCode::RouteRedirectStatusInvalid,
        ),
        (
            "redirect={to='https://www.example.com/'}",
            ApiErrorCode::RouteRedirectLoop,
        ),
        (
            "redirect={to='ftp://example.com'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
        (
            "redirect={to='https://example.com/?q=1'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
        (
            "redirect={to='https://example.com/{http.request.host}'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
        (
            "redirect={to='https://user@example.com'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
        (
            "redirect={to='https://example.com:0'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
        (
            "redirect={to='https://Example.com'}",
            ApiErrorCode::RouteRedirectInvalid,
        ),
    ] {
        let errors = redirect(fields).unwrap_err();
        assert!(
            errors.0.iter().any(|e| e.code == code),
            "{fields}: {errors:?}"
        );
    }
}

#[test]
fn decoding_keeps_stored_routes_and_rejects_mixed_targets() {
    let stored: ValidatedRoute = serde_json::from_value(
        serde_json::json!({"hostname":"a.example.com","service":"web","port":80}),
    )
    .unwrap();
    assert_eq!(stored.target.to_string(), "web:80");
    let redirect_json =
        serde_json::json!({"to":"https://example.com/","status":308,"preserve_path":true});
    for target in [
        serde_json::json!({"service":"web","port":80,"redirect":redirect_json}),
        serde_json::json!({"service":"web"}),
        serde_json::json!({}),
    ] {
        let mut route = serde_json::json!({"hostname":"a.example.com"});
        route
            .as_object_mut()
            .unwrap()
            .extend(target.as_object().unwrap().clone());
        assert!(
            serde_json::from_value::<ValidatedRoute>(route.clone()).is_err(),
            "{route}"
        );
        route["environment_id"] = "test-app".into();
        route["visibility"] = "private".into();
        route["dns"] = serde_json::json!({"type":"server_addresses"});
        route["state"] = "ready".into();
        route["message"] = "".into();
        assert!(
            serde_json::from_value::<RouteStatus>(route.clone()).is_err(),
            "{route}"
        );
    }
}

#[test]
fn redirect_only_applications_have_no_ingress_network() {
    let app = parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='old'\n[[spec.routes]]\nhostname='old.example.com'\nredirect={to='https://new.example.com'}")
        .unwrap()
        .normalize(ApplicationId::parse("test-app").unwrap());
    let target = compile_application(
        &app,
        &EnvironmentId::parse("test-app").unwrap(),
        InstanceId::parse("test-instance").unwrap(),
        &ResolutionSet::default(),
    )
    .unwrap()
    .with_ingress(true);
    assert_eq!(target.networks, []);
    assert_eq!(target.routes, app.spec().routes);
}

#[test]
fn only_exposed_services_join_ingress_and_disabling_preserves_route_intent() {
    let routes = "[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=3000\n[[spec.services]]\nname='db'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'";
    let app = parse_toml(&manifest(routes))
        .unwrap()
        .normalize(ApplicationId::parse("test-app").unwrap());
    let sources = ["web", "db"]
        .into_iter()
        .map(|name| {
            (
                ServiceName::parse(name).unwrap(),
                ResolvedSource::parse_image(
                    "nginx:alpine",
                    format!("nginx@sha256:{}", "a".repeat(64)),
                )
                .unwrap(),
            )
        })
        .collect();
    let target = compile_application(
        &app,
        &EnvironmentId::parse("test-app").unwrap(),
        InstanceId::parse("test-instance").unwrap(),
        &ResolutionSet {
            sources,
            ..ResolutionSet::default()
        },
    )
    .unwrap();
    let enabled = target.clone().with_ingress(true);
    assert_eq!(enabled.networks.len(), 2);
    assert!(
        enabled
            .networks
            .iter()
            .all(crate::DesiredNetwork::has_valid_identity)
    );
    assert_eq!(
        enabled
            .services
            .iter()
            .find(|s| s.logical_name.as_str() == "web")
            .unwrap()
            .networks
            .len(),
        2
    );
    assert_eq!(
        enabled
            .services
            .iter()
            .find(|s| s.logical_name.as_str() == "db")
            .unwrap()
            .networks
            .len(),
        1
    );
    assert_eq!(enabled.clone().with_ingress(true), enabled);
    assert_eq!(enabled.with_ingress(false), target);
}
