use super::{RouteUrl, UrlCondition, UrlState};
use crate::api::{
    DnsRecordState, DnsRecords, IngressStatus, ObservedServiceView, PublicIngressStatus,
    RouteStatus,
};
use crate::manifest::{RouteTarget, ValidatedRoute, Visibility};
use crate::{Convergence, EnvironmentId, RouteName, ServiceName};

fn route() -> ValidatedRoute {
    ValidatedRoute {
        hostname: crate::manifest::RouteHostname::parse("notes.example.com")
            .expect("the route hostname is valid"),
        name: Some(RouteName::parse("web").expect("the route name is valid")),
        visibility: Visibility::Public,
        target: RouteTarget::Service {
            service: ServiceName::parse("web").expect("the service name is valid"),
            port: 3000.try_into().expect("the conversion holds"),
        },
    }
}

fn status(route: &ValidatedRoute) -> RouteStatus {
    RouteStatus {
        environment_id: EnvironmentId::parse("app-notes-01").expect("the environment id parses"),
        name: route.name.clone(),
        hostname: route.hostname.to_string(),
        visibility: route.visibility,
        dns: DnsRecords::ServerAddresses {
            addresses: Vec::new(),
        },
        dns_state: DnsRecordState::Managed,
        target: route.target.clone(),
        state: "ready".into(),
        message: "verified".into(),
    }
}

fn service(convergence: Convergence, healthy_replicas: u16) -> ObservedServiceView {
    ObservedServiceView {
        name: "web".into(),
        image: None,
        desired_replicas: 2,
        observed_replicas: 2,
        healthy_replicas,
        convergence,
        diagnostics: Vec::new(),
    }
}

/// Derives the URL with one input changed from a ready baseline.
fn derive(
    change: impl FnOnce(&mut Vec<ValidatedRoute>, &mut IngressStatus, &mut Vec<ObservedServiceView>),
) -> RouteUrl {
    let route = route();
    let (mut applied, mut ingress, mut services) = (
        vec![route.clone()],
        ingress(status(&route)),
        vec![service(Convergence::Converged, 2)],
    );
    change(&mut applied, &mut ingress, &mut services);
    RouteUrl::derive(&route, &applied, &ingress, &services)
}

/// Healthy listeners with one route status.
fn ingress(status: RouteStatus) -> IngressStatus {
    let mut ingress = IngressStatus {
        enabled: true,
        healthy: true,
        gateway: true,
        routes: vec![status],
        ..IngressStatus::default()
    };
    ingress.private.healthy = true;
    ingress
}

#[test]
fn a_url_is_ready_only_when_every_condition_holds() {
    let ready = derive(|_, _, _| {});
    assert_eq!(ready.state, UrlState::Ready);
    assert_eq!(ready.url, "https://notes.example.com");
    assert_eq!(ready.name.expect("the name is valid").as_str(), "web");
    assert_eq!(ready.pending, []);
}

fn https(message: &str) -> UrlCondition {
    UrlCondition::Https {
        message: message.into(),
    }
}

/// Asserts each case's URL waits for exactly its conditions.
fn assert_pending<const N: usize>(cases: [(&str, RouteUrl, Vec<UrlCondition>); N]) {
    for (case, url, pending) in cases {
        let state = if pending.is_empty() {
            UrlState::Ready
        } else {
            UrlState::Pending
        };
        assert_eq!(url.state, state, "{case}");
        assert_eq!(url.pending, pending, "{case}");
    }
}

#[test]
fn listener_problems_outdate_route_checks() {
    assert_pending([
        (
            "gateway failed since the route was checked",
            derive(|_, ingress, _| {
                ingress.gateway = false;
                ingress.message = "gateway down".into();
            }),
            vec![https("gateway down")],
        ),
        (
            "tunnel disconnected since the route was checked",
            derive(|_, ingress, _| {
                ingress.public = PublicIngressStatus::Tunnel {
                    id: "tunnel".into(),
                    connected: false,
                    message: "tunnel disconnected".into(),
                };
            }),
            vec![https("tunnel disconnected")],
        ),
        (
            "ingress disabled, before the route was checked",
            derive(|_, ingress, _| {
                ingress.enabled = false;
                ingress.routes.clear();
            }),
            vec![https("Ingress is disabled in daemon configuration")],
        ),
        (
            "another application's network failed",
            derive(|_, ingress, _| ingress.healthy = false),
            vec![],
        ),
    ]);
}

#[test]
fn each_condition_keeps_a_url_pending() {
    let unchecked =
        https("Not checked yet: the daemon verifies HTTPS once the gateway serves the route");
    let web = UrlCondition::Service {
        service: ServiceName::parse("web").expect("the service name is valid"),
    };
    let cases: [(&str, RouteUrl, Vec<UrlCondition>); 9] = [
        (
            "not applied by the gateway",
            derive(|applied, _, _| applied.clear()),
            vec![UrlCondition::Ingress],
        ),
        (
            "a different route was applied under the hostname",
            derive(|applied, _, _| applied[0].visibility = Visibility::Private),
            vec![UrlCondition::Ingress],
        ),
        (
            "HTTPS not verified",
            derive(|_, ingress, _| {
                ingress.routes[0].state = "pending".into();
                ingress.routes[0].message = "no certificate".into();
            }),
            vec![https("no certificate")],
        ),
        (
            "never checked",
            derive(|_, ingress, _| ingress.routes.clear()),
            vec![unchecked.clone()],
        ),
        (
            "checked before its target changed",
            derive(|_, ingress, _| {
                ingress.routes[0].target = RouteTarget::Service {
                    service: ServiceName::parse("api").expect("the service name is valid"),
                    port: 80.try_into().expect("the conversion holds"),
                };
            }),
            vec![unchecked],
        ),
        (
            "DNS records pending",
            derive(|_, ingress, _| ingress.routes[0].dns_state = DnsRecordState::Pending),
            vec![UrlCondition::Dns {
                state: DnsRecordState::Pending,
            }],
        ),
        (
            "DNS conflict",
            derive(|_, ingress, _| ingress.routes[0].dns_state = DnsRecordState::DnsConflict),
            vec![UrlCondition::Dns {
                state: DnsRecordState::DnsConflict,
            }],
        ),
        (
            "service degraded",
            derive(|_, _, services| services[0] = service(Convergence::Degraded, 1)),
            vec![web.clone()],
        ),
        (
            "service not observed",
            derive(|_, _, services| services.clear()),
            vec![web],
        ),
    ];
    assert_pending(cases);
}

#[test]
fn redirects_need_no_service_and_manual_dns_is_not_pending() {
    let mut redirect = route();
    redirect.target = RouteTarget::Redirect {
        redirect: serde_json::from_value(
            serde_json::json!({"to": "https://example.com", "status": 308, "preserve_path": true}),
        )
        .expect("the input deserializes"),
    };
    let mut status = status(&redirect);
    status.dns_state = DnsRecordState::Manual;
    let url = RouteUrl::derive(
        &redirect,
        std::slice::from_ref(&redirect),
        &ingress(status),
        &[],
    );
    assert_eq!(url.state, UrlState::Ready);
}
