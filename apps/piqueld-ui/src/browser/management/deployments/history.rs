//! An environment's deployment history, polled while its tab is shown.
use super::PAGE;
use crate::browser::management::timeline::{CompactProgress, RunningDeployment, duration};
use crate::browser::management::{EditorContext, editor};
use crate::browser::ui::{
    Icon, Tone, empty, icon, load_older, loading, notice, operation_badge, when,
};
use crate::browser::{Alive, ClientErrorExt, document_hidden, live_row};
use crate::deployments::{DeploymentFacts, DeploymentList};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{Client, DeploymentSummary, Page};

/// The deployments shown for one environment: its first page, polled, and
/// older pages appended on request.
#[derive(Clone, Copy)]
struct History {
    /// The deployments loaded so far, newest first.
    items: RwSignal<Vec<DeploymentSummary>>,
    /// Cursor of the next older page.
    cursor: RwSignal<Option<String>>,
    /// Whether an older page was appended, so polls merge instead of replacing.
    paginated: RwSignal<bool>,
    /// Why the last load failed; `None` otherwise.
    error: RwSignal<Option<String>>,
    /// Whether a page is loading.
    loading: RwSignal<bool>,
    /// Whether the first page was read, so the list is known to be empty
    /// rather than still loading.
    fetched: RwSignal<bool>,
}

impl History {
    /// Creates an empty history, before anything is loaded.
    fn new() -> Self {
        Self {
            items: RwSignal::new(Vec::new()),
            cursor: RwSignal::new(None),
            paginated: RwSignal::new(false),
            error: RwSignal::new(None),
            loading: RwSignal::new(false),
            fetched: RwSignal::new(false),
        }
    }

    /// Applies a freshly polled first page. Before any older page is loaded,
    /// the page simply replaces the list and cursor; afterwards it is merged
    /// into the older pages (see `DeploymentList::merge_newest`).
    fn merge(self, page: Page<DeploymentSummary>) {
        self.items.update(|items| {
            if self.paginated.get_untracked() {
                items.merge_newest(page.items);
            } else {
                *items = page.items;
                self.cursor.set(page.next_cursor);
            }
        });
    }

    /// Polls the first page of `environment` every 2 seconds while the
    /// editor's Deployments tab is selected and the page is visible, and
    /// right after the tab opens, following the deployment it lists running
    /// (see `RunningDeployment`). Stops when the owning view unmounts.
    fn poll(self, context: EditorContext, environment: String) {
        let active = Alive::new();
        let running = use_context::<RunningDeployment>();
        spawn_local(async move {
            while active.get() {
                let shown = context.tabs.selected.get_untracked() == "Deployments";
                if shown && !self.loading.get_untracked() && !document_hidden() {
                    let result = Client::browser()
                        .deployment_summaries(&environment, None, Some(PAGE))
                        .await;
                    if !active.get() {
                        break;
                    }
                    match result {
                        Ok(page) => {
                            if let (Some(listed), Some(running)) = (
                                page.items
                                    .iter()
                                    .find(|listed| !listed.operation.state.terminal()),
                                running,
                            ) {
                                running.saw(&listed.operation);
                            }
                            self.merge(page);
                            self.error.set(None);
                            self.fetched.set(true);
                        }
                        Err(e) => self.error.set(Some(e.user_message())),
                    }
                }
                let delay = if shown { 2000 } else { 250 };
                gloo_timers::future::TimeoutFuture::new(delay).await;
            }
        });
    }

    /// Appends the next older page of `environment`, skipping deployments
    /// already shown.
    fn load_older(self, environment: String) {
        let next = self.cursor.get_untracked();
        self.loading.set(true);
        spawn_local(async move {
            match Client::browser()
                .deployment_summaries(&environment, next.as_deref(), Some(PAGE))
                .await
            {
                Ok(page) => {
                    self.error.set(None);
                    self.paginated.set(true);
                    self.cursor.set(page.next_cursor);
                    self.items.update(|items| items.append_older(page.items));
                }
                Err(e) => self.error.set(Some(e.user_message())),
            }
            self.loading.set(false);
        });
    }
}

/// Deployment list for the editor's environment, a row per deployment
/// opening its page. The first page is polled while the deployments tab is
/// visible; older pages are appended on request without duplicating
/// deployments already shown.
#[component]
pub(in crate::browser::management) fn DeploymentHistory() -> impl IntoView {
    let context = editor();
    let history = History::new();
    let id = context.environment_id();
    history.poll(context, id.clone());
    view! {
        <section class="stack-sm" aria-label="Deployment history">
            {move || history.error.get().map(|e| notice(Tone::Bad, e))}
            {move || {
                history
                    .items
                    .with(Vec::is_empty)
                    .then(|| {
                        if history.fetched.get() {
                            empty(
                                "No deployments yet. Deploy the saved configuration to create one.",
                            )
                        } else if history.error.get().is_none() {
                            loading("Loading deployments…")
                        } else {
                            ().into_any()
                        }
                    })
            }}
            <ul
                class="list"
                aria-label="Deployments"
                hidden={move || history.items.with(Vec::is_empty)}
            >
                <For
                    each={move || history.items.get()}
                    key={|d| d.operation.id.clone()}
                    children={move |initial| {
                        let deployment = live_row(history.items, initial, |d| &d.operation.id);
                        view! { <DeploymentRow deployment={deployment} /> }
                    }}
                />
            </ul>
            {load_older(
                "Load older deployments",
                history.cursor,
                history.loading,
                move || history.load_older(id.clone()),
            )}
            <Show when={move || {
                history.cursor.get().is_none() && history.items.with(|items| !items.is_empty())
            }}>
                <p class="hint">
                    "Older deployments are removed once past the daemon's history retention."
                </p>
            </Show>
        </section>
    }
}

/// One deployment in its environment's history, opening its page: its
/// state, what brought it (see `ApplicationViewExt::deployment_title`), the
/// configuration revision and commit it deploys, and how long it ran, or
/// while the page follows it running, its progress.
#[component]
fn DeploymentRow(
    /// The deployment to show.
    deployment: Signal<DeploymentSummary>,
) -> impl IntoView {
    let context = editor();
    let running = expect_context::<RunningDeployment>();
    let id = deployment.with_untracked(|deployment| deployment.operation.id.clone());
    let href = context.deployment_href(&context.environment_id(), &id);
    let live = Signal::derive(move || running.running(Some(&id)));
    let tag = move |shown: fn(&DeploymentSummary) -> bool, label: &'static str| {
        move || {
            deployment
                .with(shown)
                .then(|| view! { <span class="tag">{label}</span> })
        }
    };
    view! {
        <li class="list-row list-row-link deployment-row">
            {move || operation_badge(deployment.get().operation.state)} <span class="title">
                <A href={href}>{move || context.deployment_title(&deployment.get().origin)}</A>
                <small>{move || deployment.with(DeploymentFacts::commit)}</small>
            </span>
            <span class="tag">
                {move || format!("Revision {}", deployment.get().operation.generation)}
            </span> {tag(|deployment| deployment.current_target, "Current target")}
            {tag(|deployment| deployment.last_successful, "Last successful")}
            <span class="meta">
                {move || match live.get() {
                    Some(live) => view! { <CompactProgress live={live} /> }.into_any(),
                    None => {
                        deployment
                            .with(|deployment| {
                                let took = deployment
                                    .duration_ms()
                                    .map(|ms| format!("{} · ", duration(ms)));
                                view! {
                                    {took}
                                    {when(deployment.operation.created_at_ms)}
                                }
                                    .into_any()
                            })
                    }
                }}
            </span> <span class="chevron" aria-hidden="true">
                {icon(Icon::ChevronRight)}
            </span>
        </li>
    }
}
