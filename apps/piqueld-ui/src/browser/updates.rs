//! The release channel the daemon follows, and whether it has a newer build:
//! a sidebar link while one is available, and a panel on the daemon status
//! page to read it, check now, and choose the channel.

use super::ui::{Icon, Tone, badge, icon, notice, when};
use super::{ClientErrorExt, access::can, confirm, dashboard_context};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::access::{GlobalPermission, Permission};
use piqueld_client::system::{
    InstallMethod, UpdateAction, UpdateRun, UpdateRunState, UpdateStatus,
};
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

/// Time between reads of the status while an update or rollback runs.
const RUN_POLL_MS: u32 = 3_000;

/// A request of the panel's that returns the update status.
type StatusRequest = std::pin::Pin<Box<dyn Future<Output = Result<UpdateStatus, ClientError>>>>;

/// The panel's state: the shared status, and the request it is making.
#[derive(Clone, Copy)]
struct Panel {
    /// The dashboard's update status, which every view of it shares.
    updates: RwSignal<Option<UpdateStatus>>,
    /// Whether an action, a check or a change, is waiting for the daemon.
    busy: RwSignal<bool>,
    /// Why the last request failed.
    error: RwSignal<Option<String>>,
    /// Whether it is following a running update or rollback.
    following: RwSignal<bool>,
}

impl Panel {
    /// Sends `request`, marking the panel busy when it is an `action`, then
    /// shows the status it returns, or why it failed. On failure it shows
    /// the last status again, which resets the channel picker to the channel
    /// the daemon follows.
    fn send(self, action: bool, request: StatusRequest) {
        self.busy.set(action);
        spawn_local(async move {
            match request.await {
                Ok(status) => {
                    self.updates.try_set(Some(status));
                    self.error.try_set(None);
                }
                Err(failure) => {
                    self.error.try_set(Some(failure.user_message()));
                    self.updates.try_update(|_| {});
                }
            }
            self.busy.try_set(false);
        });
    }

    /// Follows a running update or rollback until it finishes, or the panel
    /// closes. The daemon doesn't answer while it restarts, so failed reads
    /// are retried; a build from before piqueld updated itself answers that
    /// it has no update status, which ends it.
    fn follow_runs(self) {
        Effect::new(move |_| {
            let running = self.updates.with(|status| {
                status
                    .as_ref()
                    .and_then(|status| status.run.as_ref())
                    .is_some_and(UpdateRun::in_progress)
            });
            if !running || self.following.get_untracked() {
                return;
            }
            self.following.set(true);
            spawn_local(async move {
                loop {
                    TimeoutFuture::new(RUN_POLL_MS).await;
                    if self.following.try_get_untracked().is_none() {
                        break;
                    }
                    let status = match Client::browser().update_status().await {
                        Ok(status) => status,
                        Err(error) if error.is_not_found() => {
                            self.error.try_set(Some(
                                "piqueld now runs a version that can't report updates; reload the page"
                                    .into(),
                            ));
                            break;
                        }
                        Err(_) => continue,
                    };
                    let finished = !status.run.as_ref().is_some_and(UpdateRun::in_progress);
                    if self.updates.try_set(Some(status)).is_some() || finished {
                        break;
                    }
                }
                self.following.try_set(false);
            });
        });
    }

    /// What the rows may do, for a caller who may `operate` this host.
    fn controls(self, status: &UpdateStatus, operate: bool) -> Controls {
        let choose = Callback::new(move |value: String| {
            if let Some(channel) = Channel::parse(&value) {
                self.send(
                    true,
                    Box::pin(async move { Client::browser().set_update_channel(channel).await }),
                );
            }
        });
        let install = Callback::new(move |()| {
            let target = self
                .updates
                .get_untracked()
                .and_then(|status| status.latest)
                .map_or_else(String::new, |latest| latest.version.number);
            if confirm(&format!(
                "Update piqueld to {target}? It restarts on it, and goes back to this version if it doesn't start."
            )) {
                self.send(
                    true,
                    Box::pin(async { Client::browser().apply_update().await }),
                );
            }
        });
        let roll_back = Callback::new(move |()| {
            let target = self
                .updates
                .get_untracked()
                .and_then(|status| status.previous)
                .unwrap_or_default();
            if confirm(&format!("Go back to piqueld {target}? It restarts on it.")) {
                self.send(
                    true,
                    Box::pin(async { Client::browser().rollback_update().await }),
                );
            }
        });
        Controls {
            editable: operate && !status.channel_configured,
            act: operate && status.install == InstallMethod::Script,
            busy: self.busy,
            choose,
            install,
            roll_back,
        }
    }
}

/// What the panel's rows may do: choose the channel, and update or roll
/// back, each only when the caller may and the host can.
#[derive(Clone, Copy)]
struct Controls {
    /// Whether the channel picker shows.
    editable: bool,
    /// Whether the update and rollback buttons show: a host the install
    /// script set up, and `system:operate`.
    act: bool,
    /// Whether a request is waiting, which disables the controls.
    busy: RwSignal<bool>,
    /// Follows the channel picked.
    choose: Callback<String>,
    /// Installs the newest build, once confirmed.
    install: Callback<()>,
    /// Goes back to the previous build, once confirmed.
    roll_back: Callback<()>,
}

/// The updates panel: the running build, the channel and its newest build,
/// how to install it, and the latest update or rollback, which it follows
/// while it runs. It reloads the status when it opens, so it shows the
/// daemon's latest check.
#[component]
pub(super) fn UpdatesPanel() -> impl IntoView {
    let operate = can(Permission::Global(GlobalPermission::SystemOperate));
    let panel = Panel {
        updates: dashboard_context().signals.updates,
        busy: RwSignal::new(false),
        error: RwSignal::new(None),
        following: RwSignal::new(false),
    };
    panel.send(
        false,
        Box::pin(async { Client::browser().update_status().await }),
    );
    panel.follow_runs();
    let check = move |_| {
        panel.send(
            true,
            Box::pin(async { Client::browser().check_for_updates().await }),
        );
    };
    view! {
        <section class="card card-flush" id="updates" aria-labelledby="updates-heading">
            <header>
                <div>
                    <h3 id="updates-heading">"Updates"</h3>
                    <p>
                        {move || {
                            if panel.updates.get().is_some_and(|status| !status.checks) {
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
                        disabled={move || panel.busy.get()}
                        on:click={check}
                    >
                        {icon(Icon::Refresh)}
                        "Check now"
                    </button>
                </Show>
            </header>
            <div class="updates-notice" hidden={move || panel.error.get().is_none()}>
                {move || panel.error.get().map(|message| notice(Tone::Bad, message))}
            </div>
            {move || {
                panel
                    .updates
                    .get()
                    .map(|status| panel.controls(&status, operate.get()).rows(&status))
            }}
        </section>
    }
}

impl Controls {
    /// The panel's rows for `status`, and how to update when the caller
    /// can't from here.
    fn rows(self, status: &UpdateStatus) -> AnyView {
        let commit = status
            .current
            .short_commit()
            .map(|commit| format!("commit {commit}"));
        let how = (!self.act)
            .then(|| status.how_to_update())
            .flatten()
            .map(|how| {
                notice(
                    Tone::Info,
                    view! {
                        <strong>"To update"</strong>
                        <span class="update-how">{how}</span>
                    },
                )
            });
        let has_how = how.is_some();
        view! {
            <ul class="status-list">
                <li class="status-row">
                    <strong>"Running"</strong>
                    <code>{status.current.number.clone()}</code>
                    <span class="status-summary">{commit}</span>
                </li>
                {self.channel(status)}
                {self.newest(status)}
                {checked(status)}
                {self.previous(status)}
                {status.run.clone().map(last_run)}
            </ul>
            <div class="updates-notice" hidden={!has_how}>
                {how}
            </div>
        }
        .into_any()
    }

    /// Whether the buttons wait: for a request, or a run of the updater.
    fn waiting(self, status: &UpdateStatus) -> impl Fn() -> bool + Copy + 'static {
        let running = status.run.as_ref().is_some_and(UpdateRun::in_progress);
        let busy = self.busy;
        move || busy.get() || running
    }

    /// The channel, with a picker when the caller may change it.
    fn channel(self, status: &UpdateStatus) -> AnyView {
        let value = if self.editable {
            let (busy, choose) = (self.busy, self.choose);
            view! {
                <select
                    class="inline-select"
                    aria-label="Release channel"
                    prop:value={status.channel.clone()}
                    disabled={move || busy.get()}
                    on:change={move |event| choose.run(event_target_value(&event))}
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
        view! {
            <li class="status-row">
                <strong>"Channel"</strong>
                {value}
            </li>
        }
        .into_any()
    }

    /// The channel's newest build, with its release notes, and the button
    /// that installs it.
    fn newest(self, status: &UpdateStatus) -> AnyView {
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
        let (waiting, install) = (self.waiting(status), self.install);
        let button = (self.act && status.available).then(|| {
            view! {
                <button
                    type="button"
                    class="btn btn-sm btn-primary"
                    disabled={waiting}
                    on:click={move |_| install.run(())}
                >
                    {icon(Icon::Download)}
                    "Update"
                </button>
            }
        });
        view! {
            <li class="status-row" data-tone={tone.attr()}>
                <strong>"Newest build"</strong>
                {badge(tone, state)}
                <span class="status-summary">{newest}</span>
                {button}
            </li>
        }
        .into_any()
    }

    /// The build a rollback returns to, with the button that does.
    fn previous(self, status: &UpdateStatus) -> Option<AnyView> {
        let previous = status.previous.clone()?;
        let (waiting, roll_back) = (self.waiting(status), self.roll_back);
        let button = self.act.then(|| {
            view! {
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={waiting}
                    on:click={move |_| roll_back.run(())}
                >
                    {icon(Icon::Restore)}
                    "Roll back"
                </button>
            }
        });
        Some(
            view! {
                <li class="status-row">
                    <strong>"Previous build"</strong>
                    <code>{previous}</code>
                    <span class="status-summary"></span>
                    {button}
                </li>
            }
            .into_any(),
        )
    }
}

/// When the channel was last checked, or why that failed.
fn checked(status: &UpdateStatus) -> AnyView {
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
    view! {
        <li class="status-row">
            <strong>"Last checked"</strong>
            {checked}
        </li>
    }
    .into_any()
}

/// The latest update or rollback: where it is, and what happened.
fn last_run(run: UpdateRun) -> AnyView {
    let (tone, label) = match UpdateRunState::parse(&run.state) {
        Some(UpdateRunState::Requested | UpdateRunState::Running) => (Tone::Pending, "Running"),
        Some(UpdateRunState::Succeeded) => (Tone::Ok, "Succeeded"),
        Some(UpdateRunState::Failed) => (Tone::Bad, "Failed"),
        None => (Tone::Neutral, "Unknown"),
    };
    let what = match UpdateAction::parse(&run.action) {
        Some(UpdateAction::Rollback) => "Rollback",
        _ => "Update",
    };
    let target = run
        .version
        .map_or_else(String::new, |version| format!(" to {version}"));
    view! {
        <li class="status-row" data-tone={tone.attr()}>
            <strong>"Last update"</strong>
            {badge(tone, label)}
            <span class="status-summary">
                {format!("{what}{target}, ")}
                {when(run.finished_at_ms.unwrap_or(run.started_at_ms))}
                {run.message.map(|message| format!(": {message}"))}
            </span>
        </li>
    }
    .into_any()
}
