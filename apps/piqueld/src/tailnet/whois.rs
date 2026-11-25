//! Identifies who a connection through the tailnet node came from, so the
//! audit trail can name them and tokens can be bound to them.
use super::COMMAND_TIMEOUT;
use async_trait::async_trait;
use piqueld_config::tailscale::LocalApi;
use piqueld_core::tailnet::TailnetPeer;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// How long a looked-up identity is reused for its address.
const CACHE_FOR: Duration = Duration::from_mins(1);
/// Lookups run on the request path, so they give up quickly.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Addresses remembered at once; the cache starts over beyond this.
const CACHE_LIMIT: usize = 1024;

/// A lookup that did not finish in time: lookups of the address were backed
/// up, or `tailscaled` did not answer. The peer is unknown rather than
/// mismatched, so callers report the lookup as unavailable.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("tailnet peer lookup timed out")]
pub struct LookupTimedOut;

/// Resolves the tailnet identity behind a peer address. Routers serving the
/// tailnet node or the dashboard socket carry one as an
/// `Arc<dyn TailnetLookup>` extension; requests on other listeners have no
/// tailnet identity.
#[async_trait]
pub trait TailnetLookup: Send + Sync {
    /// Who `peer` is, or `None` when `tailscaled` cannot tell. `fresh` skips
    /// any cache: authorization must not act on an identity that has since
    /// changed, such as a removed tag.
    ///
    /// # Errors
    /// Returns [`LookupTimedOut`] when the lookup did not finish in time.
    async fn whois(
        &self,
        peer: SocketAddr,
        fresh: bool,
    ) -> Result<Option<TailnetPeer>, LookupTimedOut>;
}

/// [`TailnetLookup`] through the node's `tailscaled`, asking its `LocalAPI`.
///
/// Identities are cached per address for [`CACHE_FOR`] to describe audited
/// requests; failed lookups are not cached. Each address is looked up by one
/// request at a time, and requests that waited for it reuse its
/// answer, fresh ones too when it started after they asked. A current cached
/// answer never waits for a lookup, and lookups of different addresses never
/// wait for each other.
pub struct Whois {
    api: LocalApi,
    peers: parking_lot::Mutex<HashMap<IpAddr, Arc<Slot>>>,
}

/// One address's last successful lookup and its single-flight lock.
#[derive(Default)]
struct Slot {
    answer: parking_lot::Mutex<Option<Answer>>,
    /// Held while the address is looked up.
    querying: Mutex<()>,
}

/// One address's last successful lookup.
struct Answer {
    /// When the lookup started: the identity is current as of then.
    asked: Instant,
    peer: TailnetPeer,
}

impl Slot {
    /// The last answer, if current for a request made at `asked`: started
    /// since then when `fresh`, else younger than [`CACHE_FOR`].
    fn current(&self, fresh: bool, asked: Instant) -> Option<TailnetPeer> {
        let answer = self.answer.lock();
        let answer = answer.as_ref()?;
        let current = if fresh {
            answer.asked >= asked
        } else {
            answer.asked.elapsed() < CACHE_FOR
        };
        current.then(|| answer.peer.clone())
    }
}

impl Whois {
    /// Looks peers up through the `tailscaled` listening on `socket`, such as
    /// the apps node's.
    #[must_use]
    pub fn at(socket: std::path::PathBuf) -> Self {
        Self {
            api: LocalApi::new(socket, COMMAND_TIMEOUT),
            peers: parking_lot::Mutex::default(),
        }
    }

    /// The answer slot of `address`. The cache starts over once it holds
    /// [`CACHE_LIMIT`] addresses.
    fn slot(&self, address: IpAddr) -> Arc<Slot> {
        let mut peers = self.peers.lock();
        if peers.len() >= CACHE_LIMIT && !peers.contains_key(&address) {
            peers.clear();
        }
        Arc::clone(peers.entry(address).or_default())
    }
}

#[async_trait]
impl TailnetLookup for Whois {
    async fn whois(
        &self,
        peer: SocketAddr,
        fresh: bool,
    ) -> Result<Option<TailnetPeer>, LookupTimedOut> {
        let (address, asked) = (peer.ip(), Instant::now());
        let slot = self.slot(address);
        if let Some(peer) = slot.current(fresh, asked) {
            return Ok(Some(peer));
        }
        let Ok(_turn) = tokio::time::timeout(LOOKUP_TIMEOUT, slot.querying.lock()).await else {
            tracing::warn!(%address, "tailnet peer lookups are backed up");
            return Err(LookupTimedOut);
        };
        if let Some(peer) = slot.current(fresh, asked) {
            return Ok(Some(peer));
        }
        let started = Instant::now();
        match tokio::time::timeout(LOOKUP_TIMEOUT, self.api.whois(address)).await {
            Ok(Ok(peer)) => {
                *slot.answer.lock() = Some(Answer {
                    asked: started,
                    peer: peer.clone(),
                });
                Ok(Some(peer))
            }
            Ok(Err(error)) => {
                tracing::warn!(?error, %address, "could not identify tailnet peer");
                Ok(None)
            }
            Err(_) => {
                tracing::warn!(%address, "tailnet peer lookup timed out");
                Err(LookupTimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A current cached identity is returned at once, even while a lookup
    /// of the same address is running.
    #[tokio::test]
    async fn cached_identities_never_wait_for_a_lookup() {
        let whois = Whois::at("/nonexistent/tailscaled.sock".into());
        let peer = SocketAddr::from(([100, 64, 0, 1], 40_000));
        let slot = whois.slot(peer.ip());
        let cached = TailnetPeer {
            login: Some("alice@example.com".into()),
            node: "laptop".into(),
            tags: Vec::new(),
        };
        *slot.answer.lock() = Some(Answer {
            asked: Instant::now(),
            peer: cached.clone(),
        });
        let _running = slot.querying.lock().await;
        let found = tokio::time::timeout(Duration::from_millis(100), whois.whois(peer, false))
            .await
            .expect("the cached identity waited for the running lookup");
        assert_eq!(found.expect("the provider has records"), Some(cached));
    }

    /// A known peer is identified and an unknown one isn't, through the
    /// `LocalAPI` alone: no `tailscale` process is started.
    #[tokio::test]
    async fn peers_are_identified_through_the_local_api() {
        let fake = piqueld_config::tailscale::FakeLocalApi::serve(&[
            (
                "/localapi/v0/whois?addr=100.64.0.1",
                200,
                r#"{"Node":{"Name":"laptop.tail1234.ts.net.","Tags":null},
                "UserProfile":{"LoginName":"alice@example.com"}}"#,
            ),
            (
                "/localapi/v0/whois?addr=100.64.0.9",
                404,
                "no match for IP:port\n",
            ),
        ])
        .expect("serve succeeds");
        let whois = Whois::at(fake.socket().to_owned());
        let known = whois
            .whois(SocketAddr::from(([100, 64, 0, 1], 40_000)), true)
            .await
            .expect("whois succeeds")
            .expect("the value is present");
        assert_eq!(known.login.as_deref(), Some("alice@example.com"));
        assert_eq!(known.node, "laptop");
        let unknown = whois
            .whois(SocketAddr::from(([100, 64, 0, 9], 40_000)), true)
            .await
            .expect("whois succeeds");
        assert_eq!(unknown, None);
    }
}
