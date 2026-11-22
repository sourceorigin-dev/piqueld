//! Live server-sent event streams, such as the event history and an
//! operation's timeline: how they are served, paced, and end.
use super::lease::{Ended, Lease};
use axum::response::{
    Sse,
    sse::{Event as ServerEvent, KeepAlive},
};
use futures_util::Stream;
use piqueld_core::api::StreamMessage;
use std::{convert::Infallible, time::Duration};
use tokio::{sync::watch, time::Instant};

/// Least time between two reads of one live stream: a burst of writes, such
/// as a deployment's, wakes a stream at most this often.
const PACE: Duration = Duration::from_millis(100);

/// A live stream's messages, read as they are sent.
pub(super) trait Follow: Send + 'static {
    /// The next message, or `None` once the stream ended.
    fn next(&mut self) -> impl Future<Output = Option<ServerEvent>> + Send;

    /// Serves this stream's messages as server-sent events until it ends.
    fn serve(self) -> Sse<impl Stream<Item = Result<ServerEvent, Infallible>> + Send + 'static>
    where
        Self: Sized,
    {
        let stream = futures_util::stream::unfold(self, |mut follow| async move {
            let message = follow.next().await?;
            Some((Ok(message), follow))
        });
        Sse::new(stream).keep_alive(KeepAlive::default())
    }
}

/// A new message named `name`.
pub(super) fn named(name: StreamMessage) -> ServerEvent {
    ServerEvent::default().event(name.as_str())
}

/// `message` named `name` with `data` as JSON, or a `stream_error` message
/// when `data` does not serialize.
pub(super) fn json(
    message: ServerEvent,
    name: StreamMessage,
    data: impl serde::Serialize,
) -> ServerEvent {
    message
        .event(name.as_str())
        .json_data(data)
        .unwrap_or_else(|_| named(StreamMessage::StreamError).data("serialization failed"))
}

impl Ended {
    /// The final message of a stream that ended this way: `access_ended`,
    /// whose data is the `ErrorBody` of the request that would now be
    /// refused, or none when the daemon shuts down.
    pub(super) fn message(self) -> Option<ServerEvent> {
        let Self::Refused(error) = self else {
            return None;
        };
        let message = || named(StreamMessage::AccessEnded);
        Some(
            message()
                .json_data(error.body())
                .unwrap_or_else(|_| message()),
        )
    }
}

/// When a live stream reads the store again: after a write to it, and at
/// most every [`PACE`].
pub(super) struct Pace {
    /// Changes after each write to the store.
    written: watch::Receiver<()>,
    /// When the store was last read.
    read_at: Instant,
}

impl Pace {
    /// Paces reads after the first, made now; `written` must be subscribed
    /// before it, so a write while it is read wakes the stream.
    pub(super) fn new(written: watch::Receiver<()>) -> Self {
        Self {
            written,
            read_at: Instant::now(),
        }
    }

    /// Waits until the store may be read again: after a write, unless
    /// `behind` (more may already be stored), or once `lease` is due for a
    /// recheck; then until [`PACE`] passed since the last read.
    pub(super) async fn wait(&mut self, lease: &Lease, behind: bool) {
        if !behind {
            lease.wait(self.written.changed()).await;
        }
        tokio::time::sleep_until(self.read_at + PACE).await;
        self.read_at = Instant::now();
    }
}
