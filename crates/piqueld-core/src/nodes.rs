//! The machines of an installation's Swarm: the daemon's host, its only
//! manager, and the worker nodes the daemon's configuration names, each by
//! its [`NodeName`] and reached through its Docker Engine at a
//! [`TcpEndpoint`].
//!
//! A worker joins the Swarm by hand. The daemon then finds which configured
//! worker it is by asking each worker's engine for its node ID, and labels
//! the node with [`NODE_LABEL`], so it knows the node by name even while the
//! machine is off.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Swarm node label naming the configured worker a node is, such as
/// `io.piqueld.node=nixosbtw`. The daemon sets it; a worker without it, or
/// with a name the configuration doesn't have, is not one of its workers.
pub const NODE_LABEL: &str = "io.piqueld.node";

crate::names::logical_name!(
    /// The name the daemon's configuration gives a worker node, such as
    /// `nixosbtw`, under `[nodes.<name>]`.
    NodeName,
    NodeNameError,
    "node names"
);

impl NodeName {
    /// The worker `labels` place a resource or a Swarm node on, by their
    /// [`NODE_LABEL`]; the host when they have none.
    #[must_use]
    pub fn labelled(labels: &BTreeMap<String, String>) -> Option<Self> {
        Self::parse(labels.get(NODE_LABEL)?).ok()
    }
}

/// A daemon on another machine, reached at `tcp://host:port`, without
/// credentials or a path: a build machine's `BuildKit` daemon, or a worker
/// node's Docker Engine. Displays as configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct TcpEndpoint(String);

impl TryFrom<String> for TcpEndpoint {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let address = value
            .strip_prefix("tcp://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest));
        // An IPv6 host is bracketed, so its port follows the last colon.
        let valid = address
            .and_then(|address| address.rsplit_once(':'))
            .is_some_and(|(host, port)| {
                !host.is_empty()
                    && !host.contains(['/', '@', '?', '#', ' '])
                    && (!host.contains(':') || host.starts_with('[') && host.ends_with(']'))
                    && port.parse::<u16>().is_ok()
            });
        if valid {
            Ok(Self(value))
        } else {
            Err("must be tcp://host:port")
        }
    }
}

impl TcpEndpoint {
    /// The daemon's `host:port`, to connect to.
    #[must_use]
    pub fn address(&self) -> &str {
        let address = self.0.strip_prefix("tcp://").unwrap_or(&self.0);
        address.strip_suffix('/').unwrap_or(address)
    }
}

impl std::fmt::Display for TcpEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_is_a_tcp_host_and_port() {
        for (endpoint, address) in [
            (
                "tcp://build.example.ts.net:1234",
                "build.example.ts.net:1234",
            ),
            ("tcp://100.64.0.1:1234/", "100.64.0.1:1234"),
            ("tcp://[fd7a::1]:1234", "[fd7a::1]:1234"),
        ] {
            let parsed = TcpEndpoint::try_from(endpoint.to_owned()).unwrap();
            assert_eq!(parsed.address(), address);
        }
        for invalid in [
            // No port, another scheme, credentials, a path, and a bare IPv6 host.
            "tcp://build.example.ts.net",
            "ssh://build.example.ts.net:22",
            "tcp://user@build.example:1234",
            "tcp://build.example:1234/buildkit",
            "tcp://fd7a::1:1234",
            "tcp://:1234",
        ] {
            assert!(
                TcpEndpoint::try_from(invalid.to_owned()).is_err(),
                "{invalid}"
            );
        }
    }
}
