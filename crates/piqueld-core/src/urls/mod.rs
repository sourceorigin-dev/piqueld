//! The URLs of an environment's rendered routes and whether each is ready,
//! derived only from state the daemon already observes. Clients read the
//! result; none probes a URL or computes readiness itself.
use crate::api::{DnsRecordState, IngressStatus, ObservedServiceView, PublicIngressStatus};
use crate::manifest::{RouteTarget, ValidatedRoute, Visibility};
use crate::names::string_enum;
use crate::{Convergence, RouteName, ServiceName};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

string_enum! {
    #[serde]
    /// Whether a URL is reachable. A known URL is not a ready one.
    pub enum UrlState {
        /// At least one condition in `pending` holds.
        Pending => "pending",
        /// Served by the gateway over verified HTTPS, with its DNS records in
        /// place and its service healthy.
        Ready => "ready",
    }
}

/// A condition that keeps a URL pending.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum UrlCondition {
    /// The gateway has not acknowledged this exact route yet.
    Ingress,
    /// HTTPS, with the route's certificate, is not verified for this route:
    /// its listener is unhealthy now, or the daemon's latest route check
    /// failed or has not run since the route changed.
    Https {
        /// Why, from the route's status.
        message: String,
    },
    /// The hostname's managed DNS records are not in place.
    Dns {
        /// `pending` or `dns_conflict`.
        state: DnsRecordState,
    },
    /// The route's service is not observed healthy.
    Service {
        /// The route's service.
        service: ServiceName,
    },
}

/// What the condition waits for: `ingress`, `HTTPS (<message>)`,
/// `DNS pending`, or `service web`.
impl std::fmt::Display for UrlCondition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ingress => formatter.write_str("ingress"),
            Self::Https { message } => write!(formatter, "HTTPS ({message})"),
            Self::Dns { state } => write!(formatter, "DNS {state}"),
            Self::Service { service } => write!(formatter, "service {service}"),
        }
    }
}

/// One URL of a rendered route, and whether it is ready.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RouteUrl {
    /// The route's name, when it has one.
    #[serde(default)]
    pub name: Option<RouteName>,
    /// `https://` and the rendered hostname.
    pub url: String,
    /// Effective visibility: `private` URLs are reachable only from the tailnet.
    pub visibility: Visibility,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: RouteTarget,
    /// `ready` exactly when `pending` is empty.
    pub state: UrlState,
    /// Every condition that keeps the URL pending, in the order above.
    pub pending: Vec<UrlCondition>,
}

impl RouteUrl {
    /// Derives a rendered route's URL and state. It is ready when:
    ///
    /// 1. the gateway acknowledged this exact route (`applied`);
    /// 2. the listener serving its visibility is enabled and healthy now
    ///    (`ingress`), and
    ///    the daemon's latest check of this route (same hostname, visibility,
    ///    and target in `ingress.routes`) found it `ready`: DNS resolves to
    ///    that listener, which serves it with a trusted certificate;
    /// 3. its DNS records are not `pending` or `dns_conflict`;
    /// 4. its service, unless it redirects, is observed healthy.
    #[must_use]
    pub fn derive(
        route: &ValidatedRoute,
        applied: &[ValidatedRoute],
        ingress: &IngressStatus,
        services: &[ObservedServiceView],
    ) -> Self {
        let status = ingress.routes.iter().find(|status| {
            status.hostname == route.hostname.as_str()
                && status.visibility == route.visibility
                && status.target == route.target
        });
        let mut pending = Vec::new();
        if !applied.contains(route) {
            pending.push(UrlCondition::Ingress);
        }
        // A disabled listener never serves the route, checked or not. A
        // listener failure observed since the route's check outdates it;
        // other applications' network failures do not.
        let listener = match (route.visibility, &ingress.public) {
            _ if !ingress.enabled => Some("Ingress is disabled in daemon configuration"),
            (Visibility::Private, _) if !ingress.private.enabled => {
                Some("Private ingress is disabled in daemon configuration")
            }
            (Visibility::Public, _) if !ingress.gateway => Some(ingress.message.as_str()),
            (
                Visibility::Public,
                PublicIngressStatus::Tunnel {
                    connected: false,
                    message,
                    ..
                },
            ) => Some(message.as_str()),
            (Visibility::Public, _) => None,
            (Visibility::Private, _) => {
                (!ingress.private.healthy).then_some(ingress.private.message.as_str())
            }
        };
        let https = match status {
            _ if listener.is_some_and(str::is_empty) => Some("Its listener is unavailable".into()),
            _ if let Some(message) = listener => Some(message.to_owned()),
            Some(status) if status.state == "ready" => None,
            Some(status) => Some(status.message.clone()),
            None => Some(
                "Not checked yet: the daemon verifies HTTPS once the gateway serves the route"
                    .into(),
            ),
        };
        if let Some(message) = https {
            pending.push(UrlCondition::Https { message });
        }
        if let Some(status) = status
            && matches!(
                status.dns_state,
                DnsRecordState::Pending | DnsRecordState::DnsConflict
            )
        {
            pending.push(UrlCondition::Dns {
                state: status.dns_state,
            });
        }
        if let Some(service) = route.target.service()
            && !services
                .iter()
                .any(|observed| observed.name == service.as_str() && observed.healthy())
        {
            pending.push(UrlCondition::Service {
                service: service.clone(),
            });
        }
        Self {
            name: route.name.clone(),
            url: format!("https://{}", route.hostname),
            visibility: route.visibility,
            target: route.target.clone(),
            state: if pending.is_empty() {
                UrlState::Ready
            } else {
                UrlState::Pending
            },
            pending,
        }
    }

    /// What keeps it pending, e.g. `ingress, DNS pending`; empty once ready.
    #[must_use]
    pub fn waiting_for(&self) -> String {
        self.pending
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl ObservedServiceView {
    /// Whether the runtime reports every desired replica running and healthy.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.convergence == Convergence::Converged && self.healthy_replicas >= self.desired_replicas
    }
}

#[cfg(test)]
mod tests;
