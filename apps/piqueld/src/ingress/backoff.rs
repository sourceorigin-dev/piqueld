//! Retrying failed work later, with a delay that grows with each failure.

/// How long to wait after each failure: `first_ms`, doubling up to `max_ms`.
#[derive(Clone, Copy)]
pub(super) struct BackoffPolicy {
    pub(super) first_ms: i64,
    pub(super) max_ms: i64,
}

/// Consecutive failures of a retried task, and when it may run again.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Backoff {
    failures: u32,
    /// No attempt is made before this time.
    retry_at_ms: i64,
}

impl Backoff {
    /// Whether an attempt may be made at `now_ms`. A pass reads the clock
    /// a little later each time, so an attempt due within a second is ready
    /// rather than left to the next pass.
    pub(super) fn ready(self, now_ms: i64) -> bool {
        now_ms + 1000 >= self.retry_at_ms
    }

    /// Records a failure at `failed_at_ms`, delaying the next attempt as
    /// `policy` says.
    pub(super) fn fail(&mut self, policy: BackoffPolicy, failed_at_ms: i64) {
        self.failures += 1;
        let delay = policy.first_ms << (self.failures - 1).min(10);
        self.retry_at_ms = failed_at_ms + delay.min(policy.max_ms);
    }
}
