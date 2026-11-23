//! Container output for one application or one of its services, followed
//! live.
use super::super::logs::{LogKind, LogViewer, StreamFilter};
use super::super::stream::EventStream;
use super::super::ui::{Icon, Tone, badge, icon, notice};
use super::super::{Alive, ClientErrorExt, document_hidden};
use super::editor;
use crate::log_output::LogLine;
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{Client, LogRecord, StreamMessage, paths};
use std::collections::VecDeque;

/// The most lines the card keeps; older ones scroll away.
const MAX_LINES: usize = 1000;
/// How often lines that arrived are drawn, so a chatty service redraws the
/// list a few times a second rather than every frame.
const FLUSH: std::time::Duration = std::time::Duration::from_millis(250);

/// Where the card's live output stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// Reading the latest output, then opening the stream.
    Connecting,
    /// New output appears as it is written.
    Live,
    /// The operator paused it; the shown output stays.
    Paused,
    /// The stream ended: it was refused, or the browser gave up on it.
    Stopped,
}

impl Follow {
    /// The badge saying where it stands.
    fn badge(self) -> AnyView {
        match self {
            Self::Connecting => badge(Tone::Pending, "Connecting"),
            Self::Live => badge(Tone::Info, "Live"),
            Self::Paused => badge(Tone::Neutral, "Paused"),
            Self::Stopped => badge(Tone::Bad, "Stopped"),
        }
    }

    /// Whether the button resumes it, rather than pausing it.
    fn resumes(self) -> bool {
        matches!(self, Self::Paused | Self::Stopped)
    }
}

/// Runtime log card for the editor's application: the latest 200 lines from
/// the last hour, then new output as Docker writes it, including that of
/// tasks a deployment starts, keeping the newest [`MAX_LINES`]. Changing the
/// service or stream filter, or resuming, reads the latest lines again. The
/// service selector lists the services of the manifest the shown environment
/// deploys; `fixed_service` scopes it to one service and hides the selector.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn ApplicationLogs(#[prop(optional)] fixed_service: Option<String>) -> impl IntoView {
    let context = editor();
    let records = RwSignal::new(VecDeque::<LogRecord>::new());
    let pending = StoredValue::new_local(VecDeque::<LogRecord>::new());
    let truncated = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let follow = RwSignal::new(Follow::Connecting);
    let scoped = fixed_service.is_some();
    let service = RwSignal::new(fixed_service.unwrap_or_default());
    let stream = RwSignal::new(None::<piqueld_client::LogStream>);
    let live = StoredValue::new_local(None::<EventStream>);
    // Bumped to read the latest output again; a read for an older one is
    // discarded.
    let generation = RwSignal::new(0_u64);
    let alive = Alive::new();
    let id = context.environment_id();
    // Whether hiding the tab closed the stream, which showing it reopens.
    let hidden = StoredValue::new(false);
    // Lines arriving within a `FLUSH` are drawn together.
    let flush = move || {
        let arrived = pending.try_update_value(std::mem::take).unwrap_or_default();
        records.try_update(|records| {
            records.extend(arrived);
            let excess = records.len().saturating_sub(MAX_LINES);
            records.drain(..excess);
        });
    };
    let stream_url = paths::stream_environment_logs(&id);
    let open = move |after: Option<String>| {
        let filters = [
            (
                "service",
                Some(service.get_untracked()).filter(|s| !s.is_empty()),
            ),
            (
                "stream",
                stream.get_untracked().map(|s| s.as_str().to_owned()),
            ),
            ("after", after),
        ];
        let query = filters
            .into_iter()
            .filter_map(|(name, value)| Some(format!("{name}={}", value?)))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            stream_url.clone()
        } else {
            format!("{stream_url}?{query}")
        };
        let opened = EventStream::open(
            &url,
            &[StreamMessage::Log, StreamMessage::AccessEnded],
            move |name, data| {
                if name == StreamMessage::AccessEnded {
                    error.set(Some("You can no longer read these logs.".into()));
                    follow.set(Follow::Stopped);
                    live.set_value(None);
                    return;
                }
                let Ok(record) = serde_json::from_str::<LogRecord>(data) else {
                    return;
                };
                let first = pending
                    .try_update_value(|pending| {
                        if pending.len() == MAX_LINES {
                            pending.pop_front();
                        }
                        pending.push_back(record);
                        pending.len() == 1
                    })
                    .unwrap_or(false);
                if first {
                    set_timeout(flush, FLUSH);
                }
            },
            move || {
                error.set(Some(
                    "The log stream closed: the daemon may be busy or restarting. Resume to follow it again."
                        .into(),
                ));
                follow.set(Follow::Stopped);
            },
        );
        follow.set(if opened.is_some() {
            Follow::Live
        } else {
            Follow::Stopped
        });
        live.set_value(opened);
    };
    // Reads the latest output, then follows what is written after it.
    let start = move || {
        live.set_value(None);
        generation.update(|generation| *generation += 1);
        let current = generation.get_untracked();
        follow.set(Follow::Connecting);
        let filter = (service.get_untracked(), stream.get_untracked());
        let (alive, id, open) = (alive.clone(), id.clone(), open.clone());
        spawn_local(async move {
            let result = Client::browser()
                .filtered_environment_logs(
                    &id,
                    (!filter.0.is_empty()).then_some(filter.0.as_str()),
                    200,
                    3600,
                    filter.1,
                )
                .await;
            if !alive.get() || generation.get_untracked() != current {
                return;
            }
            match result {
                Ok(logs) => {
                    let after = logs.items.last().map(|newest| newest.timestamp.clone());
                    truncated.set(logs.truncated);
                    pending.set_value(VecDeque::new());
                    records.set(logs.items.into());
                    error.set(None);
                    open(after);
                }
                Err(e) => {
                    error.set(Some(e.user_message()));
                    follow.set(Follow::Stopped);
                }
            }
        });
    };
    let start = StoredValue::new_local(start);
    // A hidden tab draws nothing and holds one of the daemon's few log
    // streams, so hiding it closes the stream and showing it reads the latest
    // lines again. A stream the operator paused stays paused.
    let visibility = window_event_listener(ev::visibilitychange, move |_| {
        if document_hidden() {
            if matches!(follow.get_untracked(), Follow::Connecting | Follow::Live) {
                live.set_value(None);
                generation.update(|generation| *generation += 1);
                hidden.set_value(true);
            }
        } else if hidden.get_value() {
            hidden.set_value(false);
            start.with_value(|start| start());
        }
    });
    on_cleanup(move || visibility.remove());
    Effect::new(move |_| {
        let _ = (service.get(), stream.get());
        start.with_value(|start| start());
    });
    let toggle = move |_| {
        if follow.get_untracked().resumes() {
            start.with_value(|start| start());
        } else {
            live.set_value(None);
            follow.set(Follow::Paused);
        }
    };
    let lines = Memo::new(move |_| {
        records.with(|records| LogLine::runtime(records.iter().cloned().collect()))
    });
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>{if scoped { "Service logs" } else { "Application logs" }}</h3>
                    <p>
                        "The latest 200 lines from the last hour, then new output as Docker writes it, including that of tasks a deployment starts."
                    </p>
                </div>
                <div class="btn-group">
                    {move || follow.get().badge()}
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || follow.get() == Follow::Connecting}
                        on:click={toggle}
                    >
                        {move || if follow.get().resumes() {
                            view! { {icon(Icon::Refresh)} "Resume" }.into_any()
                        } else {
                            "Pause".into_any()
                        }}
                    </button>
                </div>
            </header>
            <div class="toolbar">
                <Show when={move || !scoped}>
                    <label class="field">
                        <span>"Service"</span>
                        <select
                            prop:value={move || service.get()}
                            on:change={move |e| service.set(event_target_value(&e))}
                        >
                            <option value="">"All services"</option>
                            {context.environment_service_options()}
                        </select>
                    </label>
                </Show>
                <StreamFilter stream={stream} />
            </div>
            <div class="stack-sm">
                <Show when={move || stream.get().is_some()}>
                    <p class="hint">
                        "Merged terminal output is only included when both streams are selected."
                    </p>
                </Show>
                {move || error.get().map(|e| notice(Tone::Bad, e))}
                <Show when={move || truncated.get()}>
                    {notice(
                        Tone::Warn,
                        "The latest output was truncated. Filter by service or stream to narrow it.",
                    )}
                </Show>
            </div>
            <LogViewer
                lines={lines}
                label="Application log output"
                empty="No output in the last hour yet. New output appears here as it is written."
                kind={if scoped { LogKind::Service } else { LogKind::Application }}
            />
        </section>
    }
}
