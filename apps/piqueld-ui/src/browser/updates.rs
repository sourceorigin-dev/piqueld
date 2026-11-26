//! The release channel the daemon follows, and whether it has a newer build:
//! a sidebar link while one is available, and a panel on the daemon status
//! page to read it, check now, and choose the channel.

use super::ui::{Icon, Tone, badge, icon, notice, when};
use super::{ClientErrorExt, access::can, dashboard_context};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::access::{GlobalPermission, Permission};
use piqueld_client::system::UpdateStatus;
use piqueld_client::version::Channel;
use piqueld_client::{Client, ClientError};

/// Loads the update status once the caller may read it, into the dashboard's
/// shared signal; the panel replaces it after each check.
pub(super) fn load_updates() {
    let updates = dashboard_context().signals.updates;
    let system = can(Permission::Global(GlobalPermission::SystemRead));
    Effect::new(move |loaded: Option<bool>| {
        if loaded == Some(true) || !system.get() {
            return loaded.unwrap_or(false);
        }
        spawn_local(async move {
            if let Ok(status) = Client::browser().update_status().await {
                updates.try_set(Some(status));
            }
        });
        true
    });
}

/// Sidebar link to the updates panel, shown while the daemon's channel has a
/// newer build.
#[component]
pub(super) fn UpdateLink() -> impl IntoView {
    let updates = dashboard_context().signals.updates;
    move || {
        updates
            .get()
            .filter(|status| status.available)
            .and_then(|status| {
                status.latest.map(|latest| {
                    view! {
                        <A attr:class="update-link" href="/dashboard/system#updates">
                            {icon(Icon::Download)}
                            <span>
                                "Update available"
                                <small>{latest.version.number}</small>
                            </span>
                        </A>
                    }
                })
            })
    }
}

/// The updates panel: the running build, the channel and its newest build,
/// and how to install it. It reloads the status when it opens, so it shows
/// the daemon's latest check. While a request runs, an action's marks the
/// panel busy; a failed one shows why, and the last status again, which
/// resets the channel picker to the channel the daemon follows.
#[component]
pub(super) fn UpdatesPanel() -> impl IntoView {
    let updates = dashboard_context().signals.updates;
    let operate = can(Permission::Global(GlobalPermission::SystemOperate));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let run = move |action: bool,
                    request: std::pin::Pin<
        Box<dyn Future<Output = Result<UpdateStatus, ClientError>>>,
    >| {
        busy.set(action);
        spawn_local(async move {
            match request.await {
                Ok(status) => {
                    updates.try_set(Some(status));
                    error.try_set(None);
                }
                Err(failure) => {
                    error.try_set(Some(failure.user_message()));
                    updates.try_update(|_| {});
                }
            }
            busy.try_set(false);
        });
    };
    run(
        false,
        Box::pin(async { Client::browser().update_status().await }),
    );
    let check = move |_| {
        run(
            true,
            Box::pin(async { Client::browser().check_for_updates().await }),
        );
    };
    let choose = move |event| {
        if let Some(channel) = Channel::parse(&event_target_value(&event)) {
            run(
                true,
                Box::pin(async move { Client::browser().set_update_channel(channel).await }),
            );
        }
    };
    view! {
        <section class="card card-flush" id="updates" aria-labelledby="updates-heading">
            <header>
                <div>
                    <h3 id="updates-heading">"Updates"</h3>
                    <p>
                        {move || {
                            if updates.get().is_some_and(|status| !status.checks) {
                                "piqueld checks its release channel only when you ask: updates.check is off. It never updates itself."
                            } else {
                                "piqueld checks its release channel for newer builds every six hours. It never updates itself."
                            }
                        }}
                    </p>
                </div>
                <Show when={move || operate.get()}>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || busy.get()}
                        on:click={check}
                    >
                        {icon(Icon::Refresh)}
                        {move || if busy.get() { "Checking…" } else { "Check now" }}
                    </button>
                </Show>
            </header>
            <div class="updates-notice" hidden={move || error.get().is_none()}>
                {move || error.get().map(|message| notice(Tone::Bad, message))}
            </div>
            {move || {
                updates
                    .get()
                    .map(|status| {
                        let editable = operate.get() && !status.channel_configured;
                        rows(&status, editable, busy, choose)
                    })
            }}
        </section>
    }
}

/// The panel's rows for `status`, with a channel picker when `editable`.
fn rows(
    status: &UpdateStatus,
    editable: bool,
    busy: RwSignal<bool>,
    choose: impl Fn(leptos::ev::Event) + 'static,
) -> AnyView {
    let commit = status
        .current
        .short_commit()
        .map(|commit| format!("commit {commit}"));
    let channel = if editable {
        view! {
            <select
                class="inline-select"
                aria-label="Release channel"
                prop:value={status.channel.clone()}
                disabled={move || busy.get()}
                on:change={choose}
            >
                <option value="stable">"Stable: maintainers' releases"</option>
                <option value="nightly">"Nightly: main, every night"</option>
            </select>
        }
        .into_any()
    } else {
        let set_by = status
            .channel_configured
            .then_some("Set by updates.channel in the daemon configuration");
        view! {
            <strong class="channel-name">{status.channel.clone()}</strong>
            <span class="status-summary">{set_by}</span>
        }
        .into_any()
    };
    let (tone, state) = match (&status.latest, status.available) {
        (Some(_), true) => (Tone::Info, "Update available"),
        (Some(_), false) => (Tone::Ok, "Up to date"),
        (None, _) => (Tone::Neutral, "Not checked"),
    };
    let newest = status.latest.as_ref().map(|latest| {
        view! {
            <code>{latest.version.number.clone()}</code>
            " "
            <a href={latest.notes_url.clone()} target="_blank" rel="noreferrer">
                "Release notes"
            </a>
        }
    });
    let checked = match (status.checked_at_ms, &status.error) {
        (_, Some(error)) => view! {
            {badge(Tone::Bad, "Failed")}
            <span class="status-summary">{error.clone()}</span>
        }
        .into_any(),
        (Some(at), None) => view! { <span class="status-summary">{when(at)}</span> }.into_any(),
        (None, None) => {
            let never = if status.checks {
                "Not yet"
            } else {
                "Never: updates.check is off in the daemon configuration"
            };
            view! { <span class="status-summary">{never}</span> }.into_any()
        }
    };
    let how = status.how_to_update().map(|how| {
        notice(
            Tone::Info,
            view! {
                <strong>"To update"</strong>
                <span class="update-how">{how}</span>
            },
        )
    });
    view! {
        <ul class="status-list">
            <li class="status-row">
                <strong>"Running"</strong>
                <code>{status.current.number.clone()}</code>
                <span class="status-summary">{commit}</span>
            </li>
            <li class="status-row">
                <strong>"Channel"</strong>
                {channel}
            </li>
            <li class="status-row" data-tone={tone.attr()}>
                <strong>"Newest build"</strong>
                {badge(tone, state)}
                <span class="status-summary">{newest}</span>
            </li>
            <li class="status-row">
                <strong>"Last checked"</strong>
                {checked}
            </li>
        </ul>
        <div class="updates-notice" hidden={!status.available}>
            {how}
        </div>
    }
    .into_any()
}
