//! The deploy, promote, stop and revision actions of an environment page's
//! header, and the dialogs choosing an environment to deploy or promote.
use super::plan::{DeploymentPreview, PromotionPlan};
use crate::browser::management::editor;
use crate::browser::ui::{Icon, Modal, Tone, icon, notice, text_field};
use crate::browser::{ClientErrorExt, confirm};
use crate::deployments::Promotion;
use crate::editor::Choice;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    ApplyApplicationRequest, Client, EnvironmentId, ManifestRevision, PlanView,
    access::AppPermission,
};

/// "Preview" and "Deploy" buttons for the editor's target environment. Preview
/// asks the daemon to plan the manifest that environment deploys (its last
/// fetched one when it follows a branch; cleared whenever the saved view or
/// target changes); Deploy starts a deployment of the saved
/// generation, then shows the environment's deployments. A target following
/// a branch also offers Deploy revision (see `DeployRevision`). Without a single target
/// (an application with several environments or none), Deploy lists the
/// environments to choose one. A promoted target, which never builds,
/// offers Promote from its source instead. Either also offers Promote,
/// before its own action, into the environments promoted from it. On its own page, Deploy
/// doesn't name the environment. Preview is shown with `apps:write`, the
/// others with `apps:deploy`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(in crate::browser::management) fn DeploymentActions() -> impl IntoView {
    let context = editor();
    let preview = RwSignal::new(None::<PlanView>);
    let navigate = StoredValue::new(use_navigate());
    let promoted = Memo::new(move |_| {
        context
            .selected_environment()
            .is_some_and(|environment| environment.source.promoted_from().is_some())
    });
    // Environments promoted from this one, which its page promotes into.
    let dependents = Signal::derive(move || {
        let current = context.environment.get();
        context.saved.with(|saved| {
            saved
                .environments
                .iter()
                .filter(|environment| {
                    !environment.delete_intent
                        && environment
                            .source
                            .promoted_from()
                            .map(EnvironmentId::as_str)
                            == current.as_deref()
                })
                .map(|environment| environment.id.to_string())
                .collect::<Vec<_>>()
        })
    });
    // Promotes into those environments, from any environment of a chain.
    let promote_into = move || {
        view! {
            <Show when={move || !dependents.with(Vec::is_empty)}>
                <ChooseEnvironment
                    label="Promote"
                    title="Promote into an environment"
                    environments={dependents}
                />
            </Show>
        }
    };
    // Deploys the shown environment, building its images again on `rebuild`.
    let deploy = move |rebuild| {
        let (environment, navigate) = (context.environment_id(), navigate.get_value());
        context.deploy(environment.clone(), None, rebuild, move || {
            context.show_deployments(&environment, navigate);
        });
    };
    let inspect = move |_| {
        let Some(manifest) = context.next_manifest() else {
            return;
        };
        let app = context.saved.get_untracked();
        let request = ApplyApplicationRequest {
            manifest: manifest.to_manifest(),
            expected_generation: Some(app.generation),
            expected_application_id: Some(app.application.id().to_string()),
        };
        let environment = context.environment_id();
        context.busy.set(true);
        context.set_error(None);
        spawn_local(async move {
            match Client::browser()
                .plan_application(&request, Some(&environment))
                .await
            {
                Ok(plan) => preview.set(Some(plan)),
                Err(error) => context.set_error(Some(error.user_message())),
            }
            context.busy.set(false);
        });
    };
    Effect::new(move |_| {
        context.saved.track();
        context.environment.track();
        preview.set(None);
    });
    view! {
        <Show
            when={move || context.environment.get().is_some()}
            fallback={move || {
                let environments = Signal::derive(move || {
                    context
                        .saved
                        .with(|saved| {
                            saved
                                .environments
                                .iter()
                                .map(|environment| environment.id.to_string())
                                .collect::<Vec<_>>()
                        })
                });
                view! {
                    // Without environments, Deploy leads to creating one.
                    <Show
                        when={move || !environments.with(Vec::is_empty)}
                        fallback={move || {
                            view! {
                                <button
                                    type="button"
                                    class="btn btn-primary"
                                    title="Create an environment to deploy"
                                    hidden={move || !context.can(AppPermission::Deploy)}
                                    on:click={move |_| context.tabs.selected.set("Environments")}
                                >
                                    {icon(Icon::Rocket)}
                                    "Deploy"
                                </button>
                            }
                        }}
                    >
                        <ChooseEnvironment
                            label="Deploy"
                            title="Deploy an environment"
                            environments={environments}
                        />
                    </Show>
                }
            }}
        >
            <StopAction />
            <Show
                when={move || !promoted.get()}
                fallback={move || {
                    view! {
                        {promote_into()}
                        <PromoteAction environment={context.environment_id()} />
                    }
                }}
            >
                <button
                    type="button"
                    class="btn"
                    hidden={move || !context.can(AppPermission::Write)}
                    disabled={move || {
                        context.environment_action_blocked() || context.next_manifest().is_none()
                    }}
                    on:click={inspect}
                    title={move || {
                        if context.next_manifest().is_some() {
                            "Show what deploying this environment's manifest would change"
                        } else {
                            "Deploy this environment to fetch its manifest from its branch first"
                        }
                    }}
                >
                    {icon(Icon::Eye)}
                    "Preview"
                </button>
                {promote_into()}
                // A one-time revision overrides the branch the manifest is fetched from.
                <Show when={move || {
                    context.can(AppPermission::Deploy)
                        && context
                            .selected_environment()
                            .is_some_and(|environment| environment.source.branch().is_some())
                }}>
                    <DeployRevision />
                </Show>
                // A deployment reuses the images of unchanged builds.
                <Show when={move || {
                    context.can(AppPermission::Deploy) && context.builds_from_git()
                }}>
                    <button
                        type="button"
                        class="btn"
                        title="Deploy and build every image again, with newer base images, instead of reusing the images of unchanged builds"
                        disabled={move || context.environment_action_blocked()}
                        on:click={move |_| deploy(true)}
                    >
                        {icon(Icon::Refresh)}
                        "Rebuild"
                    </button>
                </Show>
                <button
                    type="button"
                    class="btn btn-primary"
                    hidden={move || !context.can(AppPermission::Deploy)}
                    disabled={move || context.environment_action_blocked()}
                    on:click={move |_| deploy(false)}
                >
                    {icon(Icon::Rocket)}
                    // Its own page names the environment already.
                    {move || match context.selected_environment() {
                        Some(environment) if !context.environment_page() => {
                            format!("Deploy to {}", environment.name)
                        }
                        _ => "Deploy".into(),
                    }}
                </button>
            </Show>
        </Show>
        <DeploymentPreview preview={preview} />
    }
}

/// Which revision of the manifest repository `DeployRevision` deploys, as
/// chosen in its form.
#[derive(Clone, Copy, PartialEq)]
enum RevisionKind {
    /// The head of a branch.
    Branch,
    /// A full commit hash.
    Commit,
}

impl Choice for RevisionKind {
    const ALL: &'static [Self] = &[Self::Branch, Self::Commit];

    fn value(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Commit => "commit",
        }
    }
}

impl RevisionKind {
    /// The label of the input naming the revision.
    const fn label(self) -> &'static str {
        match self {
            Self::Branch => "Branch name",
            Self::Commit => "Commit hash",
        }
    }

    /// The revision `input` names, trimmed; none while blank.
    fn revision(self, input: &str) -> Option<ManifestRevision> {
        let input = input.trim().to_owned();
        (!input.is_empty()).then_some(match self {
            Self::Branch => ManifestRevision::Branch(input),
            Self::Commit => ManifestRevision::Commit(input),
        })
    }
}

/// "Deploy revision" button and dialog for the editor's target environment,
/// which follows a branch: deploys the manifest on another branch or commit
/// of the application's repository once, without saving either, then shows
/// the environment's deployments.
#[component]
fn DeployRevision() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let kind = RwSignal::new(RevisionKind::Branch);
    let input = RwSignal::new(String::new());
    let navigate = StoredValue::new(use_navigate());
    let revision = move || input.with(|input| kind.get().revision(input));
    // The branch the environment follows.
    let current = Memo::new(move |_| {
        context
            .selected_environment()
            .and_then(|environment| environment.source.branch().cloned())
    });
    // Each opening starts from a blank form, without earlier errors.
    let open = move |_| {
        kind.set(RevisionKind::Branch);
        input.set(String::new());
        context.set_error(None);
        opened.set(true);
    };
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let Some(revision) = untrack(revision) else {
            return;
        };
        // Validated here for a precise error; the daemon validates it again.
        if let Some(current) = current.get_untracked()
            && let Err(error) = current.at(&revision)
        {
            context.set_error(Some(error.to_string()));
            return;
        }
        let (environment, navigate) = (context.environment_id(), navigate.get_value());
        context.deploy(environment.clone(), Some(revision), false, move || {
            opened.set(false);
            context.show_deployments(&environment, navigate);
        });
    };
    view! {
        <button
            type="button"
            class="btn"
            title="Deploy another branch or commit once, without changing what this environment follows"
            disabled={move || context.environment_action_blocked()}
            on:click={open}
        >
            {icon(Icon::GitBranch)}
            "Deploy revision"
        </button>
        <Modal title="Deploy a branch or commit" opened={opened} busy={context.busy}>
            <form class="stack-sm" on:submit={submit}>
                <fieldset class="stack-sm" disabled={move || context.environment_action_blocked()}>
                    <p class="hint">
                        {move || {
                            format!(
                                "Deploys the manifest on another branch or commit of the application's repository once, without saving it. The environment still follows {}: its next deployment, or a push when it deploys on push, returns to it.",
                                current.get().map_or_default(|branch| branch.to_string()),
                            )
                        }}
                    </p>
                    <div class="form-grid">
                        <label class="field">
                            <span>"Deploy"</span>
                            <select
                                prop:value={move || kind.get().value()}
                                on:change={move |event| {
                                    if let Some(choice) = RevisionKind::parse(
                                        &event_target_value(&event),
                                    ) {
                                        kind.set(choice);
                                    }
                                }}
                            >
                                <option value="branch">"The head of a branch"</option>
                                <option value="commit">"A commit"</option>
                            </select>
                        </label>
                        {move || text_field(kind.get().label(), input)}
                    </div>
                </fieldset>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || {
                            context.environment_action_blocked() || revision().is_none()
                        }}
                    >
                        {icon(Icon::Rocket)}
                        "Deploy"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// Stop, or Start once stopped, for the editor's target environment, once
/// it has been deployed; not for previews. Stopping, after confirmation,
/// withdraws its routes and scales its services to zero until it is started
/// or deployed again. Hidden without `apps:deploy`.
#[component]
fn StopAction() -> impl IntoView {
    let context = editor();
    let stopped = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.stopped())
    };
    // Previews are deleted instead.
    let deployed = move || {
        context.can(AppPermission::Deploy)
            && context.selected_environment().is_some_and(|environment| {
                environment.preview().is_none() && environment.resolved_generation.is_some()
            })
    };
    let toggle = move |_| {
        let stopping = !stopped();
        if stopping
            && !confirm(&format!(
                "Stop environment {}? Its routes are withdrawn and its services stop, keeping their configuration and volumes, until it is started or deployed again.",
                context.selected_name()
            ))
        {
            return;
        }
        context.set_stopped(context.environment_id(), stopping);
    };
    view! {
        <Show when={deployed}>
            <button
                type="button"
                class="btn"
                disabled={move || context.environment_action_blocked()}
                title={move || {
                    if stopped() {
                        "Run this environment's services and serve its routes again"
                    } else {
                        "Stop this environment's services and withdraw its routes"
                    }
                }}
                on:click={toggle}
            >
                {move || if stopped() { "Start" } else { "Stop" }}
            </button>
        </Show>
    }
}

/// The `environment` ID's own action: Deploy for one that builds its
/// source, or Promote for a promoted one (see `PromoteAction`). Either shows
/// its deployments once accepted, then closes `dialog`. It reads the
/// environment from the saved view, so a refresh updates it without closing
/// an open promotion preview. Hidden without `apps:deploy`.
#[component]
pub(in crate::browser::management) fn EnvironmentAction(
    environment: String,
    #[prop(optional)] compact: bool,
    #[prop(default = None)] dialog: Option<RwSignal<bool>>,
) -> impl IntoView {
    let context = editor();
    let id = StoredValue::new(environment);
    let promoted = Memo::new(move |_| {
        context.saved.with(|saved| {
            saved
                .deployable(&id.get_value())
                .is_some_and(|environment| environment.source.promoted_from().is_some())
        })
    });
    let navigate = StoredValue::new(use_navigate());
    let label = move || format!("Deploy to {}", context.environment_name(&id.get_value()));
    move || {
        if promoted.get() {
            return view! {
                <PromoteAction environment={id.get_value()} compact={compact} dialog={dialog} />
            }
            .into_any();
        }
        view! {
            <button
                type="button"
                class={if compact { "btn btn-sm" } else { "btn btn-primary" }}
                aria-label={label}
                hidden={move || !context.can(AppPermission::Deploy)}
                disabled={move || context.action_blocked() || context.deleting(&id.get_value())}
                on:click={move |_| {
                    let (id, navigate) = (id.get_value(), navigate.get_value());
                    context
                        .deploy(
                            id.clone(),
                            None,
                            false,
                            move || {
                                context.show_deployments(&id, navigate);
                                if let Some(dialog) = dialog {
                                    dialog.set(false);
                                }
                            },
                        );
                }}
            >
                {icon(Icon::Rocket)}
                {move || if compact { "Deploy".to_owned() } else { label() }}
            </button>
        }
        .into_any()
    }
}

/// A `label`led button opening a dialog that lists the `environments` IDs, each
/// with its own action (see `EnvironmentAction`), so one can be chosen:
/// "Deploy" over an application's environments, "Promote" over the
/// environments promoted from the shown one. Closes once an action was
/// accepted. Hidden without `apps:deploy`.
#[component]
fn ChooseEnvironment(
    label: &'static str,
    title: &'static str,
    environments: Signal<Vec<String>>,
) -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    view! {
        <button
            type="button"
            class="btn btn-primary"
            hidden={move || !context.can(AppPermission::Deploy)}
            disabled={move || environments.with(Vec::is_empty)}
            on:click={move |_| opened.set(true)}
        >
            {icon(Icon::Rocket)}
            {label}
        </button>
        <Modal title={title} opened={opened} busy={context.busy}>
            <ul class="list">
                <For
                    each={move || environments.get()}
                    key={String::clone}
                    children={move |id| {
                        let name = {
                            let id = id.clone();
                            move || context.environment_name(&id)
                        };
                        view! {
                            <li class="list-row">
                                <span class="title">{name}</span>
                                <EnvironmentAction environment={id} compact=true dialog={Some(opened)} />
                            </li>
                        }
                    }}
                />
            </ul>
        </Modal>
    }
}

/// "Promote" button for a promoted environment, or with `release` "Deploy"
/// of that earlier release into it. It plans the promotion and shows that
/// plan; confirming promotes exactly what was reviewed (the source deployment
/// it planned, which the daemon refuses if the source has moved on since, or
/// the release), then shows the environment's deployments. Confirmation
/// stays disabled while the release mounts secrets the environment lacks or
/// may not use. `compact` is the smaller list-row button; `dialog` closes once
/// the promotion was accepted. Hidden without `apps:deploy`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(in crate::browser::management) fn PromoteAction(
    environment: String,
    #[prop(optional)] release: Option<String>,
    #[prop(optional)] compact: bool,
    #[prop(default = None)] dialog: Option<RwSignal<bool>>,
) -> impl IntoView {
    let context = editor();
    let environment = StoredValue::new(environment);
    let promotion = StoredValue::new(Promotion::new(release));
    let plan = RwSignal::new(None::<PlanView>);
    let opened = RwSignal::new(false);
    let navigate = use_navigate();
    let verb = promotion.with_value(Promotion::verb);
    // A promotion names its source; deploying a release names its target.
    let label = move || {
        let target = environment.get_value();
        let source = context.saved.with(|saved| {
            saved
                .deployable(&target)
                .and_then(|environment| environment.source.promoted_from().cloned())
        });
        match source.filter(|_| verb == "Promote") {
            Some(source) => format!("Promote from {}", context.environment_name(source.as_str())),
            None => format!("{verb} to {}", context.environment_name(&target)),
        }
    };
    let inspect = move |_| {
        context.busy.set(true);
        context.set_error(None);
        let (target, request) = (
            environment.get_value(),
            promotion.with_value(Promotion::request),
        );
        spawn_local(async move {
            match Client::browser().plan_promotion(&target, &request).await {
                Ok(view) => {
                    plan.set(Some(view));
                    opened.set(true);
                }
                Err(error) => context.set_error(Some(error.user_message())),
            }
            context.busy.set(false);
        });
    };
    // The request promoting exactly what was reviewed, if it can be promoted.
    let promotable = move || {
        plan.with(|plan| promotion.with_value(|promotion| promotion.confirmed(plan.as_ref()?)))
    };
    let confirm = move |_| {
        let Some(request) = promotable() else {
            return;
        };
        let (navigate, target) = (navigate.clone(), environment.get_value());
        let sent = target.clone();
        context.mutate(
            move |client| {
                let (request, target) = (request.clone(), sent.clone());
                async move { client.promote_environment(&target, &request, false).await }
            },
            move |_| {
                opened.set(false);
                plan.set(None);
                context
                    .notice
                    .set("Promotion accepted. Follow its progress below.".into());
                context.show_deployments(&target, navigate);
                context.refresh_dashboard();
                if let Some(dialog) = dialog {
                    dialog.set(false);
                }
            },
        );
    };
    view! {
        <button
            type="button"
            class={if compact { "btn btn-sm" } else { "btn btn-primary" }}
            aria-label={label}
            hidden={move || !context.can(AppPermission::Deploy)}
            disabled={move || context.action_blocked() || context.deleting(&environment.get_value())}
            on:click={inspect}
        >
            {icon(Icon::Rocket)}
            {label}
        </button>
        <Modal
            title="Promotion preview"
            opened={opened}
            wide=true
            busy={context.busy}
            on_close={Callback::new(move |()| plan.set(None))}
        >
            {move || plan.get().map(|plan| view! { <PromotionPlan plan={plan} /> })}
            {move || context.error.get().map(|error| notice(Tone::Bad, error))}
            <div class="form-actions">
                <button
                    type="button"
                    class="btn btn-primary"
                    // A failed promotion stays blocked until inspected again.
                    disabled={move || {
                        context.action_blocked() || context.error.with(Option::is_some)
                            || promotable().is_none()
                    }}
                    on:click={confirm}
                >
                    {icon(Icon::Rocket)}
                    {format!("{verb} release")}
                </button>
            </div>
        </Modal>
    }
}
