//! Per-peer admission limits for public ceremony/device starts, shared by all
//! listeners. There is deliberately no daemon-wide cap: one caller must not be
//! able to block sign-in for everyone else. The daemon is never exposed to the
//! internet; pending state stays bounded by the ceremony and device capacities.
use super::{AuthError, Result};
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

/// Length of each fixed admission window.
const WINDOW: Duration = Duration::from_mins(1);
/// Starts admitted per peer bucket within one window.
const PER_PEER: u32 = 30;

/// Start count for one peer bucket since `started`.
struct Window {
    started: Instant,
    count: u32,
}
impl Window {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            count: 0,
        }
    }
}

/// Fixed-window counters keyed by peer bucket.
#[derive(Default)]
pub(super) struct Throttle {
    peers: HashMap<Option<IpAddr>, Window>,
}
impl Throttle {
    /// Counts one start for `peer`, failing with [`AuthError::Busy`] once its
    /// window is full. Expired windows are dropped on every call.
    ///
    /// Unix sockets share the `None` bucket. Never trust caller-supplied
    /// forwarding headers.
    pub(super) fn admit(&mut self, peer: Option<IpAddr>, now: Instant) -> Result<()> {
        self.peers
            .retain(|_, window| now.duration_since(window.started) < WINDOW);
        let window = self
            .peers
            .entry(peer.map(bucket))
            .or_insert_with(|| Window::new(now));
        if window.count >= PER_PEER {
            return Err(AuthError::Busy);
        }
        window.count += 1;
        Ok(())
    }
}

/// Groups IPv6 peers by /64, the smallest prefix normally assigned to one
/// client, so rotating addresses within it cannot multiply the allowance or
/// look like a new network.
pub(crate) fn bucket(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(_) => peer,
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || IpAddr::V6((address.to_bits() & !u128::from(u64::MAX)).into()),
            IpAddr::V4,
        ),
    }
}

#[cfg(test)]
mod tests;
