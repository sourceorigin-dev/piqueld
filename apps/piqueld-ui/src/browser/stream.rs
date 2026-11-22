//! Server-sent event streams over the browser's `EventSource`.
use leptos::wasm_bindgen::{JsCast, closure::Closure};
use piqueld_client::StreamMessage;
use std::rc::Rc;
use web_sys::{Event, EventSource, MessageEvent};

/// A listener for one kind of message.
type Listener = Closure<dyn FnMut(MessageEvent)>;

/// An open server-sent event stream, authenticated by the session cookie
/// like every dashboard request. The browser reconnects it after a dropped
/// connection on its own; dropping it closes the stream.
pub(super) struct EventStream {
    source: EventSource,
    listeners: Vec<(StreamMessage, Listener)>,
    _error: Closure<dyn FnMut(Event)>,
}

impl EventStream {
    /// Opens `url`, calling `message` with the name and data of each message
    /// named one of `names`, and `failed` once the browser gave up on the
    /// stream: it was refused, or ended in a way the browser does not retry.
    /// `None` when the browser refused to open it.
    pub(super) fn open(
        url: &str,
        names: &[StreamMessage],
        message: impl Fn(StreamMessage, &str) + 'static,
        failed: impl Fn() + 'static,
    ) -> Option<Self> {
        let source = EventSource::new(url).ok()?;
        let message = Rc::new(message);
        let listeners = names
            .iter()
            .map(|&name| {
                let message = Rc::clone(&message);
                let listener = Listener::new(move |event: MessageEvent| {
                    message(name, &event.data().as_string().unwrap_or_default());
                });
                // Only fails for a listener that is not a function.
                let _ = source.add_event_listener_with_callback(
                    name.as_str(),
                    listener.as_ref().unchecked_ref(),
                );
                (name, listener)
            })
            .collect();
        let error = Closure::<dyn FnMut(Event)>::new({
            let source = source.clone();
            move |_| {
                // Errors also report each reconnection the browser tries.
                if source.ready_state() == EventSource::CLOSED {
                    failed();
                }
            }
        });
        source.set_onerror(Some(error.as_ref().unchecked_ref()));
        Some(Self {
            source,
            listeners,
            _error: error,
        })
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.source.set_onerror(None);
        for (name, listener) in &self.listeners {
            let _ = self.source.remove_event_listener_with_callback(
                name.as_str(),
                listener.as_ref().unchecked_ref(),
            );
        }
        self.source.close();
    }
}
