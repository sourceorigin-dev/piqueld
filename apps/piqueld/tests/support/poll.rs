//! Waiting for a real Docker Engine, which converges in the background and
//! signals nothing to await. In-process work is awaited by its events
//! instead, such as `Store::wait_until`.

use std::time::Duration;

/// Runs `check` every `interval` until it returns `Some`, giving up with
/// `None` once `timeout` passed.
pub async fn eventually<T>(
    timeout: Duration,
    interval: Duration,
    mut check: impl AsyncFnMut() -> Option<T>,
) -> Option<T> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(value) = check().await {
                return value;
            }
            // wall-clock: Docker converges on its own and signals nothing;
            // check again after a pause.
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .ok()
}
