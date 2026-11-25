//! One deployment's page: its progress and timeline, details, snapshot and
//! attempts, and Restore.
use super::snapshot::DeploymentSnapshot;
use crate::application::DEPLOYMENT_TABS;
use crate::browser::format::timestamp;
use crate::browser::management::timeline::{
    DeploymentProgress, LiveTimeline, OperationTimeline, status_pill,
};
use crate::browser::management::{EditorContext, EditorFeedback, back_link, editor};
use crate::browser::ui::{Icon, Tone, empty, icon, notice, operation_badge, when};
use crate::browser::{Alive, ClientErrorExt, confirm, operation_events, release_link};
use crate::deployments::{Attempts, DeploymentFacts};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    AcceptedOperation, Client, ClientError, DeploymentView, Operation, access::AppPermission,
};

/// The deployment a deployment page shows, read as the page opens and again
/// once it ended.
#[derive(Clone, Copy)]
struct ShownDeployment {
    deployment: RwSignal<Option<DeploymentView>>,
    /// Whether the environment's latest deployment is a newer one: one that
    /// failed before replacing the current target leaves Restore offered on
    /// it.
    newer: RwSignal<bool>,
    error: RwSignal<Option<String>>,
}

impl ShownDeployment {
    fn new() -> Self {
        Self {
            deployment: RwSignal::new(None),
            newer: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }

    /// Reads deployment `operation` of `environment`, and whether a newer
    /// deployment is the environment's latest, from its detail read alongside
    /// rather than the polled one, which can trail a change made elsewhere.
    /// Abandoned when the calling owner is cleaned up.
    fn load(self, environment: String, operation: String) {
        let alive = Alive::new();
        spawn_local(async move {
            let client = Client::browser();
            let (deployment, detail) = futures_util::join!(
                client.deployment(&environment, &operation),
                client.environment_detail(&environment),
            );
            if !alive.get() {
                return;
            }
            match deployment.and_then(|deployment| Ok((deployment, detail?))) {
                Ok((deployment, detail)) => {
                    self.newer
                        .set(deployment.superseded_by(detail.latest_operation.as_ref()));
                    self.deployment.set(Some(deployment));
                    self.error.set(None);
                }
                Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => self.error.set(
                    Some("This deployment is not in the environment's history.".into()),
                ),
                Err(error) => self.error.set(Some(error.user_message())),
            }
        });
    }

    /// Reads deployment `operation` of `environment`, then again for its
    /// outcome and flags whenever `live` shows it ended while the record
    /// read is still running or of an earlier attempt: a read that crossed
    /// its end, or a rerun under the same ID.
    fn follow(self, live: LiveTimeline, environment: String, operation: String) {
        self.load(environment.clone(), operation.clone());
        let stale = Memo::new(move |_| {
            let Some(ended) = live.operation().filter(|_| live.ended() == Some(true)) else {
                return false;
            };
            self.deployment.with(|deployment| {
                deployment.as_ref().is_some_and(|deployment| {
                    !deployment.operation.state.terminal()
                        || deployment.operation.attempt < ended.attempt
                })
            })
        });
        Effect::new(move |_| {
            if stale.get() {
                self.load(environment.clone(), operation.clone());
            }
        });
    }
}

/// One deployment's page: its progress, followed live over its timeline
/// stream while it runs, then its steps and events, details, configuration
/// snapshot, and attempts. A deployment that may be restored offers
/// Restore, which opens the restoring deployment's page.
#[component]
pub(in crate::browser::management) fn DeploymentPage(operation: String) -> impl IntoView {
    let context = editor();
    let tabs = context.tabs;
    let environment = context.environment_id();
    let application_href = format!("/dashboard/applications/{}", context.id());
    if context.selected_environment().is_none() {
        return view! {
            <div class="stack-sm">
                {notice(Tone::Bad, "This environment no longer exists.")}
                {back_link(application_href, "Back to the application")}
            </div>
        }
        .into_any();
    }
    let live = LiveTimeline::follow(operation.clone());
    let progress = live.progress();
    let signals = context.signals();
    Effect::new(move |_| {
        signals.detail.with(|detail| {
            if let Some(latest) = detail
                .as_ref()
                .and_then(|detail| detail.latest_operation.as_ref())
            {
                live.resume(latest);
            }
        });
    });
    let shown = ShownDeployment::new();
    shown.follow(live, environment.clone(), operation);
    let deployment = Signal::derive(move || {
        let mut deployment = shown.deployment.get()?;
        if let Some(operation) = live.operation() {
            deployment.operation = operation;
        }
        Some(deployment)
    });
    let title = Memo::new(move |_| {
        shown.deployment.with(|deployment| {
            deployment.as_ref().map_or_else(
                || "Deployment".to_owned(),
                |deployment| context.deployment_title(&deployment.origin),
            )
        })
    });
    let loaded = Memo::new(move |_| shown.deployment.with(Option::is_some));
    let subject = format!("{}/{}", context.name(), context.selected_name());
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <A href={application_href}>{move || context.name()}</A>
            {icon(Icon::ChevronRight)}
            <A href={context.deployable_href(&environment)}>{move || context.selected_name()}</A>
            {icon(Icon::ChevronRight)}
            <span>{title}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <h1>{title}</h1>
                {move || progress.with(|p| p.as_ref().map(status_pill))}
            </div>
            <div class="page-actions">
                <RestoreDeployment deployment={deployment} newer={shown.newer} />
            </div>
        </header>
        <EditorFeedback />
        <section class="card deployment-head" aria-label="Deployment progress">
            {move || deployment.get().map(|deployment| context.deployment_meta(&deployment))}
            <DeploymentProgress live={live} subject={subject} />
        </section>
        {move || shown.error.get().map(|error| notice(Tone::Bad, error))}
        {tabs.bar(&DEPLOYMENT_TABS, None)}
        {tabs.panel("Timeline", view! { <OperationTimeline live={live} /> })}
        {move || {
            if !loaded.get() {
                return None;
            }
            let initial = shown.deployment.get_untracked()?;
            let deployment = Signal::derive(move || {
                deployment.get().unwrap_or_else(|| initial.clone())
            });
            Some(
                view! { <DeploymentSections deployment={deployment} /> },
            )
        }}
    }
    .into_any()
}

/// A deployment page's tabs past its timeline: its operation's fields, the
/// configuration it captured, and its attempts, loaded once their tab opens.
#[component]
fn DeploymentSections(deployment: Signal<DeploymentView>) -> impl IntoView {
    let context = editor();
    let tabs = context.tabs;
    view! {
        {tabs
            .panel(
                "Details",
                move || deployment.with(|deployment| context.deployment_details(deployment)),
            )}
        {tabs.panel("Snapshot", view! { <DeploymentSnapshot deployment={deployment} /> })}
        {tabs
            .panel(
                "Attempts",
                view! {
                    <Show when={move || tabs.selected.get() == "Attempts"}>
                        <DeploymentAttempts deployment={deployment} />
                    </Show>
                },
            )}
    }
}

/// Restore, offered on a deployment the caller may restore that succeeded
/// and is not the current target, unless a `newer` deployment failed to
/// replace it. Opens the restoring deployment's page once accepted.
#[component]
fn RestoreDeployment(
    deployment: Signal<Option<DeploymentView>>,
    newer: RwSignal<bool>,
) -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    move || {
        let id = deployment.with(|deployment| {
            deployment
                .as_ref()
                .filter(|deployment| {
                    deployment.succeeded_at_ms.is_some()
                        && (!deployment.current_target || newer.get())
                })
                .map(|deployment| deployment.operation.id.clone())
        })?;
        let navigate = navigate.clone();
        context.can_restore().then(|| {
            view! {
                <button
                    type="button"
                    class="btn"
                    title="Deploy this deployment again exactly as it ran: its manifest, images and secret versions."
                    disabled={move || context.action_blocked()}
                    on:click={move |_| {
                        let navigate = navigate.clone();
                        context
                            .restore(
                                id.clone(),
                                move |accepted| {
                                    navigate(
                                        &context
                                            .deployment_href(
                                                &accepted.environment_id,
                                                &accepted.operation_id,
                                            ),
                                        NavigateOptions::default(),
                                    );
                                },
                            );
                    }}
                >
                    {icon(Icon::Restore)}
                    "Restore"
                </button>
            }
        })
    }
}

impl EditorContext {
    /// Whether the caller may restore deployments of the shown environment:
    /// `apps:deploy`, or `previews:deploy` in a preview.
    fn can_restore(self) -> bool {
        let preview = self
            .selected_environment()
            .is_some_and(|environment| environment.preview().is_some());
        self.can(if preview {
            AppPermission::PreviewsDeploy
        } else {
            AppPermission::Deploy
        })
    }

    /// Confirms, then restores `deployment` of the shown environment or
    /// preview exactly as it ran, and runs `restored` once accepted.
    fn restore(self, deployment: String, restored: impl FnOnce(AcceptedOperation) + 'static) {
        if !confirm(&format!(
            "Restore deployment {deployment}? Its jobs don't run again, and volume contents and migrations stay as they are."
        )) {
            return;
        }
        let Some(environment) = self.selected_environment() else {
            return;
        };
        self.mutate(
            move |client| {
                let (environment, deployment) = (environment.clone(), deployment.clone());
                async move { client.restore_deployment(&environment, &deployment).await }
            },
            move |accepted| {
                self.refresh_dashboard();
                restored(accepted);
            },
        );
    }

    /// What a deployment page's header says of `deployment`: where it came
    /// from, the commit, revision and release it deploys, when it started,
    /// and its operation's ID.
    fn deployment_meta(self, deployment: &DeploymentView) -> impl IntoView + use<> {
        let operation = &deployment.operation;
        let release = deployment.release.as_ref().map(|release| {
            view! { <span>"Release " {release_link(&self.id(), &release.to_string())}</span> }
        });
        view! {
            <p class="deployment-meta">
                <span>{self.describe_origin(&deployment.origin)}</span>
                {deployment.commit().map(|commit| view! { <span>{commit}</span> })}
                <span>{format!("Revision {}", operation.generation)}</span>
                {release}
                <span>"Created " {when(operation.created_at_ms)}</span>
                <span>"Attempt " {operation.attempt}</span>
                <code>{operation.id.clone()}</code>
            </p>
        }
    }

    /// A deployment's Details tab: its operation's fields, and why it failed.
    fn deployment_details(self, deployment: &DeploymentView) -> AnyView {
        let op = deployment.operation.clone();
        let release = deployment.release.as_ref().map_or_else(
            || "Not recorded".into_any(),
            |release| release_link(&self.id(), &release.to_string()),
        );
        view! {
            <div class="stack-sm">
                <dl class="kv">
                    <dt>"Operation ID"</dt>
                    <dd>
                        <code>{op.id}</code>
                    </dd>
                    <dt>"Release"</dt>
                    <dd>{release}</dd>
                    <dt>"Origin"</dt>
                    <dd>{self.describe_origin(&deployment.origin)}</dd>
                    <dt>"Kind"</dt>
                    <dd>{op.kind.as_str()}</dd>
                    <dt>"Configuration revision"</dt>
                    <dd>{op.generation}</dd>
                    <dt>"Attempts"</dt>
                    <dd>{op.attempt}</dd>
                    <dt>"Phase"</dt>
                    <dd>{op.phase.map_or("Waiting", piqueld_client::Phase::as_str)}</dd>
                    {op
                        .resource
                        .map(|resource| {
                            view! {
                                <dt>"Resource"</dt>
                                <dd>
                                    <code>{resource}</code>
                                </dd>
                            }
                        })}
                    <dt>"Created"</dt>
                    <dd>{timestamp(op.created_at_ms)}</dd>
                    <dt>"Updated"</dt>
                    <dd>{timestamp(op.updated_at_ms)}</dd>
                    {op
                        .finished_at_ms
                        .map(|ms| {
                            view! {
                                <dt>"Finished"</dt>
                                <dd>{timestamp(ms)}</dd>
                            }
                        })}
                </dl>
                {op
                    .error_message
                    .map(|error| {
                        notice(
                            Tone::Bad,
                            view! {
                                {op.error_code.map(|code| view! { <strong>{code}</strong> })}
                                <span>{error}</span>
                            },
                        )
                    })}
            </div>
        }
        .into_any()
    }
}

/// Paginated attempt outcomes of one deployment, plus the live attempt while it runs.
#[component]
fn DeploymentAttempts(deployment: Signal<DeploymentView>) -> impl IntoView {
    let op = deployment.get_untracked().operation;
    let attempts = RwSignal::new(Attempts::default());
    let failure = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let active = Alive::new();
    let load = Callback::new(move |next: Option<String>| {
        if loading.get_untracked() {
            return;
        }
        let environment = op.environment_id.to_string();
        let deployment_id = op.id.clone();
        let active = active.clone();
        loading.set(true);
        spawn_local(async move {
            let result = Client::browser()
                .deployment_attempts(&environment, &deployment_id, next.as_deref())
                .await;
            if !active.get() {
                return;
            }
            match result {
                Ok(page) => {
                    failure.set(None);
                    attempts.update(|attempts| attempts.show(page, next.is_some()));
                }
                Err(error) => failure.set(Some(error.user_message())),
            }
            loading.set(false);
        });
    });
    load.run(None);
    let visible_attempts = Signal::derive(move || {
        let live = deployment.get().operation;
        attempts.with(|attempts| attempts.visible(&live))
    });

    view! {
        <div class="stack-sm">
            {move || failure.get().map(|error| notice(Tone::Bad, error))}
            {move || loading.get().then(|| crate::browser::ui::loading("Loading attempts…"))}
            {move || {
                if visible_attempts.with(Vec::is_empty) {
                    (!loading.get() && failure.get().is_none())
                        .then(|| empty("No attempts yet."))
                        .into_any()
                } else {
                    view! {
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th class="num">"Attempt"</th>
                                        <th>"State"</th>
                                        <th>"Outcome"</th>
                                        <th>"Updated"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {visible_attempts
                                        .get()
                                        .into_iter()
                                        .map(|attempt| view! { <AttemptRow attempt={attempt} /> })
                                        .collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }
            }} <div class="btn-group">
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || loading.get()}
                    on:click={move |_| load.run(None)}
                >
                    {icon(Icon::Refresh)}
                    "Refresh attempts"
                </button>
                <Show when={move || attempts.with(|attempts| attempts.cursor.is_some())}>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| {
                            load.run(attempts.with_untracked(|attempts| attempts.cursor.clone()));
                        }}
                    >
                        "Older attempts"
                    </button>
                </Show>
            </div>
        </div>
    }
}

/// One attempt outcome with a link to its events and diagnostics, shown with
/// `events:read`.
#[component]
fn AttemptRow(attempt: Operation) -> impl IntoView {
    let context = editor();
    let history = operation_events(&attempt.id);
    let outcome = attempt.failure();
    view! {
        <tr>
            <td class="num">{attempt.attempt}</td>
            <td>{operation_badge(attempt.state)}</td>
            <td class="muted">{outcome}</td>
            <td class="muted">{when(attempt.updated_at_ms)}</td>
            <td class="actions">
                <Show when={move || context.can(AppPermission::EventsRead)}>
                    <A href={history.clone()}>"Events"</A>
                </Show>
            </td>
        </tr>
    }
}
