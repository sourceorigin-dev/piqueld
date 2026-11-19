//! The Swarm's nodes: this engine, its only manager, and the worker nodes the
//! daemon's configuration names, each reached through its own Docker Engine
//! (see [`piqueld_core::nodes`]).
//!
//! A worker joins by hand. Before each change, the topology check finds the
//! nodes that joined without a name, asks each configured worker's engine
//! for its node ID, and labels the node it matches with that worker's name.
//! Every other node must be a worker carrying a name the configuration has:
//! anything else fails closed with [`DockerError::IncompatibleSwarm`].
use super::{BollardDocker, BollardResult, DockerError};
use bollard::models::{Node, NodeSpecAvailabilityEnum, NodeSpecRoleEnum, Reachability};
use bollard::query_parameters::{
    DeleteNodeOptions, LeaveSwarmOptions, ListNodesOptions, UpdateNodeOptions,
};
use piqueld_core::api::{NodeState, NodeStatus, SwarmStatus};
use piqueld_core::nodes::{NODE_LABEL, NodeName, TcpEndpoint};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// The port the manager listens on for workers, Docker's default.
const SWARM_PORT: u16 = 2377;
/// Longest a worker's engine gets to answer: it may be off, and the
/// topology check runs before every change.
const WORKER_TIMEOUT: Duration = Duration::from_secs(3);

/// The Swarm's worker nodes, by name, and the address its manager listens
/// on and advertises to them.
#[derive(Default)]
pub struct Nodes {
    /// `docker.swarm_address`; without it, the Swarm is initialized on
    /// loopback, where no other machine can join it.
    address: Option<IpAddr>,
    /// Each worker's Docker Engine.
    workers: BTreeMap<NodeName, BollardDocker>,
}

impl Nodes {
    /// Workers named `workers`, each reached at its endpoint, in a Swarm on
    /// `address`.
    pub fn new(
        address: Option<IpAddr>,
        workers: impl IntoIterator<Item = (NodeName, TcpEndpoint)>,
    ) -> Self {
        Self {
            address,
            workers: workers
                .into_iter()
                .map(|(name, endpoint)| (name, BollardDocker::connect_tcp(endpoint)))
                .collect(),
        }
    }

    /// The request initializing a Swarm on `address`, its data path
    /// included, or only on loopback without one.
    fn init_request(&self) -> bollard::models::SwarmInitRequest {
        let address = self.address.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        bollard::models::SwarmInitRequest {
            listen_addr: Some(SocketAddr::new(address, SWARM_PORT).to_string()),
            advertise_addr: Some(address.to_string()),
            data_path_addr: self.address.map(|address| address.to_string()),
            ..Default::default()
        }
    }

    /// Checks that `nodes` are `own`, the Swarm's only manager, ready,
    /// reachable and active, and workers each carrying a distinct name the
    /// configuration has. Returns why they aren't.
    fn check(&self, own: &str, nodes: &[Node]) -> Result<(), String> {
        if !nodes.iter().any(|node| node.id.as_deref() == Some(own)) {
            return Err("this engine is not a node of its Swarm".into());
        }
        let mut named = BTreeMap::new();
        for node in nodes {
            let id = node.id.as_deref().unwrap_or_default();
            let role = node.spec.as_ref().and_then(|spec| spec.role);
            if id == own {
                let ready = role == Some(NodeSpecRoleEnum::MANAGER)
                    && node.spec.as_ref().and_then(|spec| spec.availability)
                        == Some(NodeSpecAvailabilityEnum::ACTIVE)
                    && Self::ready(node)
                    && node
                        .manager_status
                        .as_ref()
                        .and_then(|status| status.reachability)
                        == Some(Reachability::REACHABLE);
                if !ready {
                    return Err("this engine is not a ready, reachable, active manager".into());
                }
                continue;
            }
            let described = Self::describe(node);
            if role != Some(NodeSpecRoleEnum::WORKER) {
                return Err(format!("{described} is another manager"));
            }
            let name = Self::name(node)
                .filter(|name| self.workers.contains_key(name))
                .ok_or_else(|| {
                    format!("{described} is no worker [nodes] names, or its engine didn't answer")
                })?;
            if let Some(other) = named.insert(name.clone(), described.clone()) {
                return Err(format!("{described} and {other} are both {name}"));
            }
        }
        Ok(())
    }

    /// The worker `node` is, by its label.
    fn name(node: &Node) -> Option<NodeName> {
        let labels = node.spec.as_ref()?.labels.as_ref()?;
        NodeName::parse(labels.get(NODE_LABEL)?).ok()
    }

    /// Whether Swarm hears from `node`.
    fn ready(node: &Node) -> bool {
        node.status.as_ref().and_then(|status| status.state)
            == Some(bollard::models::NodeState::READY)
    }

    /// Names `node` in messages by its hostname and ID.
    fn describe(node: &Node) -> String {
        format!(
            "node {} ({})",
            node.description
                .as_ref()
                .and_then(|description| description.hostname.as_deref())
                .unwrap_or("without a hostname"),
            node.id.as_deref().unwrap_or_default()
        )
    }
}

impl BollardDocker {
    /// Whether this engine, a worker's, answers within [`WORKER_TIMEOUT`],
    /// or why not.
    async fn answers(&self) -> Result<(), String> {
        let client = self.client().map_err(|error| error.to_string())?;
        tokio::time::timeout(WORKER_TIMEOUT, client.ping())
            .await
            .map_err(|_| format!("no answer within {} seconds", WORKER_TIMEOUT.as_secs()))?
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// Uses `nodes` as the Swarm's workers and address.
    #[must_use]
    pub fn with_nodes(mut self, nodes: Nodes) -> Self {
        self.nodes = std::sync::Arc::new(nodes);
        self
    }

    /// Initializes this engine as the manager of a new Swarm, on
    /// `docker.swarm_address` or loopback.
    pub(super) async fn init_swarm(&self) -> Result<(), DockerError> {
        self.client()?
            .init_swarm(self.nodes.init_request())
            .await
            .or_request("initialize Docker Swarm")?;
        Ok(())
    }

    /// Leaves the Swarm, deleting every service, network, secret and config
    /// it holds, then initializes a new one on `docker.swarm_address`.
    /// Containers stop with their services; volumes and images stay. Only
    /// `piqueld swarm recreate` calls it, while the daemon is stopped.
    ///
    /// # Errors
    /// Without `docker.swarm_address`, or when Docker refuses.
    pub async fn recreate_swarm(&self) -> Result<IpAddr, DockerError> {
        let address = self
            .nodes
            .address
            .ok_or(DockerError::Validation("read docker.swarm_address"))?;
        let swarm = self.swarm_info("inspect Docker Swarm state").await?;
        if swarm.local_node_state != Some(bollard::models::LocalNodeState::INACTIVE) {
            self.client()?
                .leave_swarm(Some(LeaveSwarmOptions { force: true }))
                .await
                .or_request("leave Docker Swarm")?;
        }
        self.init_swarm().await?;
        Ok(address)
    }

    /// The address the Swarm's manager advertises to workers: where they
    /// reconnect after a restart, which Swarm never changes.
    ///
    /// # Errors
    /// When Docker can't be read, or this engine isn't the manager.
    pub async fn swarm_address(&self) -> Result<Option<IpAddr>, DockerError> {
        let own = self.local_node_id().await?;
        Ok(self
            .swarm_nodes()
            .await?
            .iter()
            .find(|node| node.id.as_deref() == Some(own.as_str()))
            .and_then(|node| node.manager_status.as_ref()?.addr.as_deref())
            .and_then(|address| address.parse::<SocketAddr>().ok())
            .map(|address| address.ip()))
    }

    /// Checks that the Swarm is one piqueld runs on (see [`Nodes::check`]),
    /// after naming the workers that joined since the last check.
    pub(super) async fn validate_topology(&self) -> Result<(), DockerError> {
        let own = self.local_node_id().await?;
        let mut nodes = self.swarm_nodes().await?;
        if self.name_workers(&own, &nodes).await? {
            nodes = self.swarm_nodes().await?;
        }
        self.nodes.check(&own, &nodes).map_err(|reason| {
            tracing::warn!(reason, "the Swarm has a node piqueld doesn't know");
            DockerError::IncompatibleSwarm
        })
    }

    /// Lists the Swarm's nodes.
    async fn swarm_nodes(&self) -> Result<Vec<Node>, DockerError> {
        self.client()?
            .list_nodes(None::<ListNodesOptions>)
            .await
            .or_request("list Swarm nodes")
    }

    /// Labels each node that joined without a name with the configured
    /// worker whose engine reports its ID. A worker that joins again gets a
    /// new ID; the node it left, down, still carries its name, and is
    /// removed. Returns whether any node changed.
    async fn name_workers(&self, own: &str, nodes: &[Node]) -> Result<bool, DockerError> {
        let unnamed = nodes
            .iter()
            .filter(|node| node.id.as_deref() != Some(own) && Nodes::name(node).is_none())
            .collect::<Vec<_>>();
        if unnamed.is_empty() {
            return Ok(false);
        }
        let ids = futures_util::future::join_all(self.nodes.workers.iter().map(
            async |(name, engine)| {
                let id =
                    tokio::time::timeout(WORKER_TIMEOUT, engine.swarm_info("identify a worker"))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .and_then(|swarm| swarm.node_id);
                (name, id)
            },
        ))
        .await;
        let mut changed = false;
        for node in unnamed {
            let Some((name, _)) = ids
                .iter()
                .find(|(_, id)| id.is_some() && id.as_deref() == node.id.as_deref())
            else {
                continue;
            };
            for earlier in nodes
                .iter()
                .filter(|other| Nodes::name(other).as_ref() == Some(*name) && !Nodes::ready(other))
            {
                self.client()?
                    .delete_node(
                        earlier.id.as_deref().unwrap_or_default(),
                        Some(DeleteNodeOptions { force: false }),
                    )
                    .await
                    .or_request("remove a worker's earlier node")?;
            }
            self.name_node(node, name).await?;
            tracing::info!(worker = %name, node = Nodes::describe(node), "matched a worker node");
            changed = true;
        }
        Ok(changed)
    }

    /// Labels `node` as the worker `name`, keeping its other settings.
    async fn name_node(&self, node: &Node, name: &NodeName) -> Result<(), DockerError> {
        let mut spec = node.spec.clone().unwrap_or_default();
        spec.labels
            .get_or_insert_default()
            .insert(NODE_LABEL.into(), name.to_string());
        let version = node
            .version
            .as_ref()
            .and_then(|version| version.index)
            .and_then(|index| i64::try_from(index).ok())
            .ok_or(DockerError::Request("read a node's version"))?;
        self.client()?
            .update_node(
                node.id.as_deref().unwrap_or_default(),
                spec,
                UpdateNodeOptions { version },
            )
            .await
            .or_request("name a worker node")
    }

    /// The Swarm and its configured workers, as status shows them; `None`
    /// when the configuration names neither a Swarm address nor workers.
    /// Each worker's engine is asked whether it answers, unless Swarm
    /// reports the node down.
    pub async fn swarm_status(&self) -> Option<SwarmStatus> {
        if self.nodes.address.is_none() && self.nodes.workers.is_empty() {
            return None;
        }
        let (address, nodes) = tokio::time::timeout(WORKER_TIMEOUT, async {
            (
                self.swarm_address().await.ok().flatten(),
                self.swarm_nodes().await.ok(),
            )
        })
        .await
        .unwrap_or_default();
        let workers = futures_util::future::join_all(self.nodes.workers.iter().map(
            async |(name, engine)| {
                let node = nodes.as_ref().map(|nodes| {
                    nodes
                        .iter()
                        .find(|node| Nodes::name(node).as_ref() == Some(name))
                });
                let state = match node {
                    None => NodeState::Unknown,
                    Some(None) => NodeState::NotJoined,
                    Some(Some(node)) if Nodes::ready(node) => NodeState::Ready,
                    Some(Some(_)) => NodeState::Down,
                };
                let checked = if state == NodeState::Down {
                    None
                } else {
                    Some(engine.answers().await)
                };
                NodeStatus {
                    name: name.to_string(),
                    docker: engine.address.to_string(),
                    node_id: node.flatten().and_then(|node| node.id.clone()),
                    state,
                    engine_reachable: checked.as_ref().map(Result::is_ok),
                    message: checked.and_then(Result::err),
                }
            },
        ))
        .await;
        Some(SwarmStatus {
            address: address.map(|address| address.to_string()),
            configured_address: self.nodes.address.map(|address| address.to_string()),
            workers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{ManagerStatus, NodeDescription, NodeSpec};

    fn node(id: &str, role: NodeSpecRoleEnum, name: Option<&str>) -> Node {
        Node {
            id: Some(id.into()),
            spec: Some(NodeSpec {
                role: Some(role),
                availability: Some(NodeSpecAvailabilityEnum::ACTIVE),
                labels: name.map(|name| [(NODE_LABEL.to_owned(), name.to_owned())].into()),
                ..Default::default()
            }),
            description: Some(NodeDescription {
                hostname: Some(format!("host-{id}")),
                ..Default::default()
            }),
            status: Some(bollard::models::NodeStatus {
                state: Some(bollard::models::NodeState::READY),
                ..Default::default()
            }),
            manager_status: (role == NodeSpecRoleEnum::MANAGER).then(|| ManagerStatus {
                reachability: Some(Reachability::REACHABLE),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_named_workers_join_this_manager() {
        let nodes = Nodes::new(
            Some("100.64.0.1".parse().unwrap()),
            [(
                NodeName::parse("nixosbtw").unwrap(),
                TcpEndpoint::try_from("tcp://nixosbtw:2375".to_owned()).unwrap(),
            )],
        );
        let manager = node("own", NodeSpecRoleEnum::MANAGER, None);
        let worker = |id, name| node(id, NodeSpecRoleEnum::WORKER, name);
        assert!(nodes.check("own", std::slice::from_ref(&manager)).is_ok());
        // A worker down, while the machine is off, is still one of them.
        let mut down = worker("w1", Some("nixosbtw"));
        down.status.as_mut().unwrap().state = Some(bollard::models::NodeState::DOWN);
        assert!(nodes.check("own", &[manager.clone(), down]).is_ok());
        for (nodes_list, reason) in [
            (vec![worker("w1", None)], "no worker"),
            (vec![worker("w1", Some("other"))], "no worker"),
            (
                vec![
                    worker("w1", Some("nixosbtw")),
                    worker("w2", Some("nixosbtw")),
                ],
                "are both nixosbtw",
            ),
            (
                vec![node("m2", NodeSpecRoleEnum::MANAGER, None)],
                "another manager",
            ),
        ] {
            let all = [vec![manager.clone()], nodes_list].concat();
            assert!(
                nodes.check("own", &all).unwrap_err().contains(reason),
                "{reason}"
            );
        }
        // This engine must be the manager, and in the Swarm.
        assert!(nodes.check("own", &[worker("own", None)]).is_err());
        assert!(nodes.check("own", &[]).is_err());
    }

    #[test]
    fn a_swarm_initializes_on_its_address_or_loopback() {
        let loopback = Nodes::default().init_request();
        assert_eq!(loopback.listen_addr.as_deref(), Some("127.0.0.1:2377"));
        assert_eq!(loopback.advertise_addr.as_deref(), Some("127.0.0.1"));
        assert_eq!(loopback.data_path_addr, None);
        let tailnet = Nodes::new(Some("100.64.0.1".parse().unwrap()), []).init_request();
        assert_eq!(tailnet.listen_addr.as_deref(), Some("100.64.0.1:2377"));
        assert_eq!(tailnet.advertise_addr.as_deref(), Some("100.64.0.1"));
        assert_eq!(tailnet.data_path_addr.as_deref(), Some("100.64.0.1"));
    }
}
