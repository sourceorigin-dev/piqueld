//! The pace of the daemon's background loops.
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Ticks every period, skipping ticks missed while work ran. The first tick
/// completes at once.
pub(super) struct Ticker(tokio::time::Interval);

impl Ticker {
    pub(super) fn new(period: Duration) -> Self {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self(interval)
    }

    /// Waits for the next tick.
    pub(super) async fn next(&mut self) {
        self.0.tick().await;
    }

    /// Waits for the next tick, returning `false` once `cancellation` is
    /// cancelled first.
    pub(super) async fn tick(&mut self, cancellation: &CancellationToken) -> bool {
        tokio::select! {
            () = cancellation.cancelled() => false,
            () = self.next() => true,
        }
    }
}
