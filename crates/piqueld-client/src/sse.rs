//! Server-sent events read from a streamed response body.

use crate::{ClientError, StreamMessage, client::MAX_RESPONSE_BODY_BYTES};
use futures_util::StreamExt;
use progenitor_client::ByteStream;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;

/// One of the daemon's live streams: its `name` messages, each decoded as a
/// `T`, such as an operation's timeline or a line of output.
pub struct LiveStream<T> {
    messages: Messages,
    name: StreamMessage,
    decoded: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> LiveStream<T> {
    /// Reads the `name` messages of a streamed response `body`.
    pub(crate) fn new(body: ByteStream, name: StreamMessage) -> Self {
        Self {
            messages: Messages::new(body),
            name,
            decoded: PhantomData,
        }
    }

    /// The next message; `Ok(None)` for a keep-alive, which the daemon sends
    /// every 15 seconds, so a caller can tell a quiet stream from a dropped
    /// one; or `None` once the stream ended, as it does when the daemon
    /// shuts down. Messages it doesn't know are skipped. Cancel-safe.
    ///
    /// # Errors
    /// Returns a transport error when the connection fails, an exchange
    /// error when the daemon can no longer read what it streams
    /// (`stream_error`), and a 403 API error with the daemon's reason once
    /// the caller lost access (`access_ended`). The stream ends after any of
    /// them.
    pub async fn next(&mut self) -> Option<Result<Option<T>, ClientError>> {
        loop {
            let message = match self.messages.next().await? {
                Ok(Received::Message(message)) => message,
                Ok(Received::Comment) => return Some(Ok(None)),
                Err(error) => return Some(Err(error)),
            };
            return Some(match StreamMessage::parse(&message.event) {
                Some(name) if name == self.name => serde_json::from_str(&message.data)
                    .map(Some)
                    .map_err(|source| ClientError::Decode { source }),
                Some(StreamMessage::StreamError) => Err(ClientError::exchange(format!(
                    "the {} stream failed: {}",
                    self.name, message.data
                ))),
                Some(StreamMessage::AccessEnded) => Err(ClientError::api(
                    http::StatusCode::FORBIDDEN,
                    message.data.as_bytes(),
                )),
                _ => continue,
            });
        }
    }
}

/// One server-sent event.
pub(crate) struct Message {
    /// Its `event` field, `message` without one.
    pub(crate) event: String,
    /// Its `data` lines, joined by newlines.
    pub(crate) data: String,
}

/// What a server-sent event stream carried next.
pub(crate) enum Received {
    /// A message.
    Message(Message),
    /// A comment, such as a keep-alive: the server is still there.
    Comment,
}

/// Reads server-sent events from a response body. Lines end in `\n` or
/// `\r\n`; fields other than `event` and `data` are skipped, and a message
/// may not exceed the response body limit.
pub(crate) struct Messages {
    body: ByteStream,
    /// Bytes received, not yet read as lines.
    buffer: Vec<u8>,
    /// The `event` field of the message read so far.
    event: Option<String>,
    /// The `data` of the message read so far, once it has some.
    data: Option<String>,
}

impl Messages {
    pub(crate) fn new(body: ByteStream) -> Self {
        Self {
            body,
            buffer: Vec::new(),
            event: None,
            data: None,
        }
    }

    /// The next message or comment, or `None` once the body ended.
    /// Cancel-safe: bytes received stay buffered for the next call.
    pub(crate) async fn next(&mut self) -> Option<Result<Received, ClientError>> {
        loop {
            while let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line = match String::from_utf8(line) {
                    Ok(line) => line,
                    Err(source) => return Some(Err(ClientError::TextDecode { source })),
                };
                if line.is_empty() {
                    let event = self.event.take();
                    if let Some(data) = self.data.take() {
                        let event = event.unwrap_or_else(|| "message".to_owned());
                        return Some(Ok(Received::Message(Message { event, data })));
                    }
                    continue;
                }
                if line.starts_with(':') {
                    return Some(Ok(Received::Comment));
                }
                let (field, value) = line
                    .split_once(':')
                    .map_or((line.as_str(), ""), |(field, value)| {
                        (field, value.strip_prefix(' ').unwrap_or(value))
                    });
                match field {
                    "event" => self.event = Some(value.to_owned()),
                    "data" => {
                        if let Some(data) = &mut self.data {
                            data.push('\n');
                            data.push_str(value);
                        } else {
                            self.data = Some(value.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            let chunk = match self.body.next().await? {
                Ok(chunk) => chunk,
                Err(error) => return Some(Err(ClientError::transport(error))),
            };
            let pending = self.buffer.len() + self.data.as_ref().map_or(0, String::len);
            if pending.saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
                return Some(Err(ClientError::exchange(format!(
                    "a server-sent event exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit"
                ))));
            }
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{Messages, Received};
    use bytes::Bytes;
    use progenitor_client::ByteStream;

    #[tokio::test]
    async fn messages_join_data_lines_across_chunks_and_skip_comments() {
        let chunks = [
            ": keep-alive\n\nevent: timeline\r\nda",
            "ta: {\"a\":\ndata:1}\nid: 7\n\n",
            "data: plain\n\nevent: ignored without data\n\n",
        ]
        .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes())));
        let mut messages = Messages::new(ByteStream::new(Box::pin(futures_util::stream::iter(
            chunks,
        ))));
        let mut read = Vec::new();
        while let Some(received) = messages.next().await {
            read.push(match received.unwrap() {
                Received::Message(message) => (message.event, message.data),
                Received::Comment => ("comment".to_owned(), String::new()),
            });
        }
        assert_eq!(
            read,
            [
                ("comment".to_owned(), String::new()),
                ("timeline".to_owned(), "{\"a\":\n1}".to_owned()),
                ("message".to_owned(), "plain".to_owned()),
            ]
        );
    }
}
