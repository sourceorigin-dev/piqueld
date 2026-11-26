//! The only place piqueld waits: every delay, backoff and poll goes through
//! [`pause`] or [`poll_until`], and clippy's `disallowed-methods` bans the raw
//! sleeps everywhere else.

use std::future::Future;
use std::time::Duration;

use tokio::time::{Instant, Sleep};

/// When a [`pause`] ends: a [`Duration`] from now, or an [`Instant`].
pub trait Until {
    /// Starts the timer that fires at this point.
    fn timer(self) -> Sleep;
}

impl Until for Duration {
    #[expect(
        clippy::disallowed_methods,
        reason = "`pause` is the one place allowed to sleep."
    )]
    fn timer(self) -> Sleep {
        tokio::time::sleep(self)
    }
}

impl Until for Instant {
    #[expect(
        clippy::disallowed_methods,
        reason = "`pause` is the one place allowed to sleep."
    )]
    fn timer(self) -> Sleep {
        tokio::time::sleep_until(self)
    }
}

/// Waits until `until`, a [`Duration`] or an [`Instant`]: a retry's backoff,
/// a poll's interval, a deadline, or a timer arm in `select!`. Returns tokio's
/// [`Sleep`], so it can be pinned, reset or raced.
pub fn pause(until: impl Until) -> Sleep {
    until.timer()
}

/// Calls `check` every `interval` until it returns `Some`, and returns that
/// value, or `None` once `timeout` has passed. `check` runs once at once, and
/// again after each `interval`; a `check` still running at the deadline is
/// dropped.
pub async fn poll_until<T, F, Fut>(timeout: Duration, interval: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    let poll = async {
        loop {
            if let Some(value) = check().await {
                return value;
            }
            pause(interval).await;
        }
    };
    tokio::time::timeout_at(deadline, poll).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn poll_until_returns_the_first_value_found() {
        let mut calls = 0;
        let found = poll_until(Duration::from_secs(10), Duration::from_secs(1), || {
            calls += 1;
            let found = (calls == 3).then_some(calls);
            async move { found }
        })
        .await;
        assert_eq!(found, Some(3));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_until_gives_up_after_the_timeout() {
        let start = Instant::now();
        let found = poll_until(Duration::from_secs(5), Duration::from_secs(1), || async {
            None::<()>
        })
        .await;
        assert_eq!(found, None);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_waits_until_a_deadline() {
        let start = Instant::now();
        pause(start + Duration::from_secs(2)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_waits_the_duration() {
        let start = Instant::now();
        pause(Duration::from_secs(3)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }
}
