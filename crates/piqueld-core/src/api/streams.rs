//! The messages of the daemon's live server-sent event streams.

use crate::names::string_enum;

string_enum! {
    /// The name of a message a live server-sent event stream sends, as its
    /// `event` field. A stream may add messages, so a reader skips names it
    /// doesn't know.
    pub enum StreamMessage {
        /// One history event, as JSON, from the event stream.
        Event => "event",
        /// The whole operation timeline, as JSON, from the timeline stream,
        /// first when it opens and then again on each change.
        Timeline => "timeline",
        /// One line of a service's output, a `LogRecord` as JSON, from the
        /// log stream. Its ID is the line's timestamp.
        Log => "log",
        /// The event stream resumed after events that history no longer
        /// holds; history must be reloaded. The stream ends.
        HistoryExpired => "history_expired",
        /// The stream could no longer read the store or Docker, or serialize
        /// what it read. The stream ends.
        StreamError => "stream_error",
        /// The caller lost access to what the stream shows; its data is the
        /// `ErrorBody` of the request that would now be refused. The stream
        /// ends.
        AccessEnded => "access_ended",
    }
}
