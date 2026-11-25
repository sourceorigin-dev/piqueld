//! The application and environment pages: their headers, tabs, and the
//! cards only they show.
use super::deployments::{self, DeploymentActions, DeploymentHistory};
use super::logs::ApplicationLogs;
use super::secrets::{EnvironmentSecrets, SecretFileSettings, StoredSecrets};
use super::settings::{MetadataSettings, NewService, RepositorySettings, VolumeSettings};
use super::{
    EditorFeedback, Page, back_link, editor, environments, jobs, links, previews, releases, routes,
    services, terminal, timeline, traffic, variables,
};
use crate::application::{APPLICATION_TABS, ApplicationViewExt, ENVIRONMENT_TABS};
use crate::browser::format::timestamp;
use crate::browser::runtime::RuntimeOverview;
use crate::browser::ui::{Icon, Tone, badge, health_badge, icon, notice};
use crate::browser::{ApplicationRow, builds, confirm, observability};
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::access::AppPermission;
use piqueld_client::paths;

/// The application page: shared configuration, its environments, and history
/// across all of them.
#[component]
pub(super) fn ApplicationSections() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
    let tabs = context.tabs;
    let id = context.id();
    let health = move || {
        let id = context.id();
        signals.applications.with(|rows| {
            rows.iter()
                .find(|row| row.application.id.as_str() == id)
                .map(ApplicationRow::health)
        })
    };
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <span>{move || context.name()}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <MetadataSettings />
                {move || health().map(health_badge)}
            </div>
            <div class="page-actions">
                <a
                    class="btn btn-ghost"
                    href={paths::download_application_manifest(&id)}
                    download
                    title="Download the saved manifest as TOML"
                >
                    {icon(Icon::Download)}
                    "Manifest"
                </a>
                <DeploymentActions />
            </div>
        </header>
        <EditorFeedback />
        {tabs.bar(&APPLICATION_TABS, Some(Callback::new(move |tab| context.tab_hidden(tab))))}
        {tabs
            .panel(
                "Overview",
                view! {
                    <div class="stack">
                        <ApplicationOverview />
                        <DeleteApplication />
                    </div>
                },
            )}
        {tabs.panel("Environments", view! { <environments::EnvironmentList /> })}
        {tabs
            .panel(
                "Previews",
                view! {
                    <Show when={move || tabs.selected.get() == "Previews"}>
                        <previews::PreviewList />
                    </Show>
                },
            )}
        <ApplicationSettings />
        {tabs
            .panel(
                "Releases",
                view! {
                    <Show when={move || tabs.selected.get() == "Releases"}>
                        <releases::ReleaseHistory application={context.id()} />
                    </Show>
                },
            )}
        {tabs
            .panel(
                "Builds",
                view! {
                    <Show when={move || tabs.selected.get() == "Builds"}>
                        <builds::BuildHistory
                            application={context.id()}
                            allowed={Callback::new(move |
                                (permission, environment): (AppPermission, String)|
                            { context.can_in(permission, Some(&environment)) })}
                        />
                    </Show>
                },
            )}
        {tabs
            .panel(
                "Events",
                view! {
                    <Show when={move || tabs.selected.get() == "Events"}>
                        <observability::EventHistory application={context.id()} />
                    </Show>
                },
            )}
    }
}

/// Identity and revision of the application.
#[component]
fn ApplicationOverview() -> impl IntoView {
    let context = editor();
    view! {
        <section class="card" aria-labelledby="application-heading">
            <header>
                <div>
                    <h3 id="application-heading">"Application"</h3>
                    <p>
                        "Configuration shared by every environment. Each environment deploys it with its own secrets, volumes, and history."
                    </p>
                </div>
            </header>
            {move || {
                context
                    .saved
                    .with(|saved| {
                        view! {
                            <dl class="kv">
                                <dt>"Application ID"</dt>
                                <dd>
                                    <code>{saved.application.id().to_string()}</code>
                                </dd>
                                <dt>"Configuration"</dt>
                                <dd>
                                    {format!(
                                        "Revision {} · {}",
                                        saved.generation,
                                        if saved.edited() {
                                            "read from Git, edited in piqueld"
                                        } else if context.repository_backed() {
                                            "read from Git"
                                        } else {
                                            "saved in piqueld"
                                        },
                                    )}
                                </dd>
                                <dt>"Environments"</dt>
                                <dd>{saved.environment_names().join(", ")}</dd>
                                <dt>"Created"</dt>
                                <dd>{timestamp(saved.created_at_ms)}</dd>
                                <dt>"Updated"</dt>
                                <dd>{timestamp(saved.updated_at_ms)}</dd>
                            </dl>
                        }
                    })
            }}
        </section>
    }
}

/// Danger-zone card that deletes the application (guarded by the saved
/// generation) after confirmation, then returns to the application list.
/// Hidden without `apps:delete`.
#[component]
fn DeleteApplication() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let delete = move |_| {
        let environments = context
            .saved
            .with_untracked(ApplicationViewExt::environment_names);
        let message = format!(
            "Delete this application, all its environments ({}) and previews, their services, and all deployment history? Environments' volumes will be detached and kept with their data; previews' volumes are removed with their data.",
            environments.join(", ")
        );
        if !confirm(&message) {
            return;
        }
        let saved = context.saved.get_untracked();
        let navigate = navigate.clone();
        context.mutate_once(
            async move |client| {
                let confirmed = environments.iter().map(String::as_str).collect::<Vec<_>>();
                client
                    .delete_application_with_preconditions(
                        saved.application.id().as_str(),
                        Some(saved.generation),
                        false,
                        &confirmed,
                    )
                    .await
            },
            move |_| {
                context.refresh_dashboard();
                navigate("/dashboard/applications", NavigateOptions::default());
            },
        );
    };
    view! {
        <section class="card card-danger" hidden={move || !context.can(AppPermission::Delete)}>
            <header>
                <div>
                    <h3>"Delete application"</h3>
                    <p>
                        "Removes every environment's and preview's running services and all configuration and deployment history. Environments' volumes are detached and kept with their data; previews' volumes are removed with their data."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.action_blocked()}
                    on:click={delete}
                >
                    "Delete application"
                </button>
            </header>
        </section>
    }
}

/// How a repository-backed application's saved configuration differs from
/// the manifest last read from its repository, with a button saving that
/// manifest again for holders of `apps:write`, and of `apps:deploy` when it
/// lets pushes deploy. Shown only while it has edits.
#[component]
fn RepositoryEdits() -> impl IntoView {
    let context = editor();
    let allowed = move || {
        context.can(AppPermission::Write)
            && (context.can(AppPermission::Deploy)
                || !context.saved.with(|saved| {
                    saved.repository.as_ref().is_some_and(|repository| {
                        repository
                            .spec()
                            .deploys_on_push(saved.application.spec().manifest.as_ref())
                    })
                }))
    };
    move || {
        let edits = context
            .saved
            .with(|saved| saved.edited().then(|| saved.edits()));
        edits.map(|edits| {
            notice(
                Tone::Info,
                view! {
                    <span>
                        "Edited since the manifest was last read from the repository. Environments following the branch the connection names deploy this configuration until a push changes the manifest there, which replaces it. Previews and other branches deploy their own branch's manifest."
                    </span>
                    <div class="stack-sm">
                        {edits.into_iter().map(deployments::change_row).collect_view()}
                    </div>
                    <div class="btn-group" hidden={move || !allowed()}>
                        <button
                            type="button"
                            class="btn btn-sm"
                            disabled={move || context.action_blocked()}
                            on:click={move |_| context.reset_edits()}
                        >
                            "Reset to repository"
                        </button>
                    </div>
                },
            )
        })
    }
}

/// Source, services, routes, volumes, jobs and secrets tabs. A
/// repository-backed application's edits are listed above them. Their forms
/// are read-only without `apps:write`; the secret store needs `secrets:write`
/// instead.
#[component]
fn ApplicationSettings() -> impl IntoView {
    let context = editor();
    let tabs = context.tabs;
    let forms = view! {
        {tabs
            .panel(
                "Services",
                view! {
                    <div class="section-header">
                        <div>
                            <h2>"Services"</h2>
                            <p>
                                "Each service runs one image as a replicated Swarm service on the application network."
                            </p>
                        </div>
                        <NewService />
                    </div>
                    <services::ServiceList />
                },
            )}
        {tabs.panel("Variables", view! { <variables::VariableSettings /> })}
        {tabs.panel("Routes", view! { <routes::RouteSettings /> })}
        {tabs.panel("Volumes", view! { <VolumeSettings /> })}
        {tabs.panel("Jobs", view! { <jobs::JobSettings /> })}
        {tabs.panel("Links", view! { <links::LinkSettings /> })}
        {tabs.panel("Secrets", view! { <SecretFileSettings /> })}
    };
    view! {
        <div hidden={move || {
            !matches!(
                tabs.selected.get(),
                "Source" | "Services" | "Variables" | "Routes" | "Volumes" | "Jobs" | "Links"
                | "Secrets"
            )
        }}>
            <div class="stack">
                {move || context.read_only_notice()} <RepositoryEdits />
                {tabs.panel("Source", view! { <RepositorySettings /> })}
                <div hidden={move || tabs.selected.get() != "Secrets"}>
                    <StoredSecrets />
                </div>
                <fieldset disabled={move || !context.can(AppPermission::Write)}>{forms}</fieldset>
            </div>
        </div>
    }
}

/// One environment or preview: its runtime, deployments, secrets, logs, and
/// history. While a deployment runs, its Overview and Deployments tabs show
/// its progress (see `timeline::RunningDeployment`). An environment's
/// overview renames and deletes it; a preview's shows its branch and
/// deletes it.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn EnvironmentPage() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
    let tabs = context.tabs;
    let id = context.environment_id();
    let preview = context
        .page
        .with_value(|page| matches!(page, Page::Preview(_)));
    let (list, kind) = if preview {
        ("previews", "preview")
    } else {
        ("environments", "environment")
    };
    let app_href = move || format!("/dashboard/applications/{}?tab={list}", context.id());
    if context.selected_environment().is_none() {
        return view! {
            <div class="stack-sm">
                {notice(Tone::Bad, format!("This {kind} no longer exists."))}
                {back_link(app_href, format!("Back to {list}"))}
            </div>
        }
        .into_any();
    }
    timeline::RunningDeployment::provide();
    let name = move || context.selected_name();
    let health = {
        let id = id.clone();
        move || {
            signals
                .environment_row(&id)
                .map(|row| row.health())
                .or_else(|| {
                    signals.detail.with(|detail| {
                        detail
                            .as_ref()
                            .filter(|detail| detail.environment.id.as_str() == id)
                            .map(|detail| {
                                ApplicationHealth::from_server_state(detail.status.state)
                                    .unless_stopped(detail.environment.stopped())
                            })
                    })
                })
        }
    };
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <A href={app_href}>{move || context.name()}</A>
            {icon(Icon::ChevronRight)}
            <span>{name}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <h1>{name}</h1>
                {move || health().map(health_badge)}
                {move || context.selected_deleting().then(|| badge(Tone::Warn, "Deleting"))}
            </div>
            <div class="page-actions">
                {if preview {
                    view! { <previews::PreviewActions /> }.into_any()
                } else {
                    view! { <DeploymentActions /> }.into_any()
                }}
            </div>
        </header>
        <EditorFeedback />
        {tabs.bar(&ENVIRONMENT_TABS, Some(Callback::new(move |tab| context.tab_hidden(tab))))}
        {tabs
            .panel(
                "Overview",
                view! {
                    <div class="stack">
                        <timeline::RunningProgress />
                        {preview.then(|| view! { <previews::PreviewSettings /> })}
                        <RuntimeOverview />
                        <variables::EnvironmentVariables />
                        {(!preview).then(|| view! { <environments::EnvironmentSettings /> })}
                        {preview.then(|| view! { <previews::DeletePreview /> })}
                    </div>
                },
            )}
        {tabs.panel("Deployments", view! { <DeploymentHistory /> })}
        {tabs.panel("Secrets", view! { <EnvironmentSecrets /> })}
        {tabs
            .panel(
                "Logs",
                view! {
                    <Show when={move || tabs.selected.get() == "Logs"}>
                        <ApplicationLogs />
                    </Show>
                },
            )}
        {tabs
            .panel(
                "Traffic",
                view! {
                    <Show when={move || tabs.selected.get() == "Traffic"}>
                        <traffic::EnvironmentTraffic />
                    </Show>
                },
            )}
        {tabs.panel("Terminal", view! { <terminal::ExecTerminal /> })}
        {tabs
            .panel(
                "Events",
                view! {
                    <Show when={move || tabs.selected.get() == "Events"}>
                        <observability::EventHistory environment={id.clone()} />
                    </Show>
                },
            )}
    }
    .into_any()
}
