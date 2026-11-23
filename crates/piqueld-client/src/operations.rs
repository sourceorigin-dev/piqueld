//! Asynchronous operation inspection, and their timelines of steps.
use crate::{
    Client, ClientError, LiveStream, Operation, StreamMessage,
    client::{EnvelopedCall, GeneratedCall},
};
use piqueld_core::timeline::Timeline;

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        self.generated.get_operation(id).data().await
    }

    /// Reads an operation's timeline: the steps it ran, is running, and has
    /// left.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation_timeline(&self, id: &str) -> Result<Timeline, ClientError> {
        self.generated.get_operation_timeline(id).data().await
    }

    /// Follows an operation's timeline as it changes. The stream has no
    /// request deadline, since it lasts as long as the caller follows it;
    /// it carries keep-alives every 15 seconds, so the caller can bound how
    /// long it waits for one.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the stream cannot be opened, including a
    /// 503 when the daemon has too many streams open.
    pub async fn operation_timeline_stream(&self, id: &str) -> Result<TimelineStream, ClientError> {
        let body = self
            .streaming()
            .generated
            .stream_operation_timeline(id)
            .payload()
            .await?;
        Ok(LiveStream::new(body, StreamMessage::Timeline))
    }
}

/// An operation's timeline as the daemon streams it: whole, at once, then
/// again each time it changes. Reconnecting sends it whole again.
pub type TimelineStream = LiveStream<Timeline>;
