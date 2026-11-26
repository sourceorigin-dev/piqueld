//! What deploying or promoting would change, as planned before confirming.
use super::variable_values;
use crate::browser::management::editor;
use crate::browser::management::releases::ReleaseCard;
use crate::browser::management::timeline::PlannedSteps;
use crate::browser::ui::{Modal, Tone, badge, notice};
use crate::changes::{ChangeDisplay, Mark};
use leptos::prelude::*;
use piqueld_client::{ManifestChange, PlanView, RuntimeCoverage, SecretProblem};

/// A promotion plan: the release and where it comes from, the volumes it
/// adds, every secret the environment lacks for it, and what deploying it
/// changes.
#[component]
pub(super) fn PromotionPlan(
    /// The promotion plan to show.
    plan: PlanView,
) -> impl IntoView {
    let context = editor();
    let release = plan.release.clone().map(|release| {
        let volumes = if release.new_volumes.is_empty() {
            "None".to_owned()
        } else {
            release.new_volumes.join(", ")
        };
        let secrets = (!release.secrets.is_empty()).then(|| {
            view! {
                <div class="stack-sm">
                    {release
                        .secrets
                        .iter()
                        .map(|problem| {
                            notice(
                                Tone::Bad,
                                match problem {
                                    SecretProblem::Missing { secret } => {
                                        format!(
                                            "Secret {secret} is missing from the application's store.",
                                        )
                                    }
                                    SecretProblem::AccessDenied { secret } => {
                                        format!("Secret {secret} does not allow this environment.")
                                    }
                                    SecretProblem::Unavailable { secret } => {
                                        format!(
                                            "Secret {secret} lost its value in key recovery; store a new version.",
                                        )
                                    }
                                    SecretProblem::Deleting { secret } => {
                                        format!(
                                            "Secret {secret} is being deleted; finish or abandon the deletion.",
                                        )
                                    }
                                },
                            )
                        })
                        .collect_view()}
                    <p class="hint">
                        "Resolve each secret problem in the Secrets tab before promoting."
                    </p>
                </div>
            }
        });
        view! {
            <section class="stack-sm">
                <div class="section-header">
                    <h3>"Release"</h3>
                </div>
                <dl class="kv">
                    <dt>"Origin"</dt>
                    <dd>{context.describe_origin(&release.origin)}</dd>
                    <dt>"New volumes"</dt>
                    <dd>{volumes}</dd>
                </dl>
                {secrets}
                {view! { <ReleaseCard release={Signal::stored(release.release)} planned=true /> }.into_any()}
            </section>
        }
    });
    view! {
        {release}
        <PlanDetails plan={plan} />
    }
}

/// Panel showing a deployment plan: configuration changes, planned runtime
/// actions and plan diagnostics.
#[component]
pub(super) fn DeploymentPreview(
    /// The plan to show; the panel closes while it is `None`.
    preview: RwSignal<Option<PlanView>>,
) -> impl IntoView {
    let opened = RwSignal::new(false);
    Effect::new(move |_| opened.set(preview.get().is_some()));
    view! {
        <Modal
            title="Deployment preview"
            opened={opened}
            wide=true
            on_close={Callback::new(move |()| preview.set(None))}
        >
            {move || {
                preview
                    .get()
                    .map(|plan| {
                        view! {
                            {plan
                                .identical
                                .then(|| {
                                    notice(
                                        Tone::Info,
                                        "Saved configuration matches the latest deployment snapshot. Deploying refreshes image resolution only.",
                                    )
                                })}
                            <p class="hint">
                                "Image tags and runtime state may change before the deployment runs."
                            </p>
                            <PlanDetails plan={plan} />
                        }
                    })
            }}
        </Modal>
    }
}

/// One manifest field change: its path, then its value before and after,
/// or line by line when the value is structured (see `ChangeDisplay`).
pub(in crate::browser::management) fn change_row(change: ManifestChange) -> impl IntoView {
    let display = ChangeDisplay::of(&change);
    let block = matches!(display, ChangeDisplay::Lines(_));
    view! {
        <div class="diff-row" class:diff-block={block}>
            <code>{change.field}</code>
            {match display {
                ChangeDisplay::Scalar { before, after } => {
                    view! {
                        <span class="before">{before.unwrap_or_else(|| "absent".into())}</span>
                        <span class="arrow" aria-hidden="true">
                            "→"
                        </span>
                        <span class="after">{after.unwrap_or_else(|| "absent".into())}</span>
                    }
                        .into_any()
                }
                ChangeDisplay::Lines(lines) => {
                    view! {
                        <pre class="diff-lines">
                            {lines
                                .into_iter()
                                .map(|line| {
                                    let class = match line.mark {
                                        Mark::Same => "diff-line",
                                        Mark::Removed => "diff-line removed",
                                        Mark::Added => "diff-line added",
                                    };
                                    view! {
                                        <span class={class}>
                                            {format!("{} {}\n", line.mark.sign(), line.text)}
                                        </span>
                                    }
                                })
                                .collect_view()}
                        </pre>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// What deploying a plan changes: configuration changes, variable values,
/// planned runtime actions, rollouts, and plan diagnostics.
#[component]
fn PlanDetails(
    /// The plan to show.
    plan: PlanView,
) -> impl IntoView {
    view! {
        <section>
            <div class="section-header">
                <h3>"Configuration changes"</h3>
            </div>
            {if plan.changes.is_empty() {
                view! { <p class="hint">"No configuration changes since the last deployment."</p> }
                    .into_any()
            } else {
                plan.changes.into_iter().map(change_row).collect_view().into_any()
            }}
        </section>
        {(!plan.variables.is_empty())
            .then(|| {
                view! {
                    <section>
                        <div class="section-header">
                            <h3>"Variables"</h3>
                        </div>
                        {variable_values(&plan.variables)}
                    </section>
                }
            })}
        <section>
            <div class="section-header">
                <h3>"Planned actions"</h3>
            </div>
            {plan.runtime_coverage.note().map(|note| view! { <p class="hint">{note}</p> })}
            {
                let steps = plan.plan.steps();
                if steps.is_empty() {
                    (plan.runtime_coverage == RuntimeCoverage::Complete)
                        .then(|| view! { <p class="hint">"No runtime actions are required."</p> })
                        .into_any()
                } else {
                    view! { <PlannedSteps steps={steps} /> }.into_any()
                }
            }
        </section>
        <section>
            <div class="section-header">
                <h3>"Rollout"</h3>
            </div>
            <ul class="stack-sm">
                {plan
                    .rollouts
                    .into_iter()
                    .map(|rollout| {
                        view! {
                            <li class="btn-group">
                                <code>{rollout.service}</code>
                                {badge(Tone::Neutral, rollout.order.as_str())}
                                <span class="hint">
                                    {format!(
                                        "{} · monitor {}s",
                                        rollout.order_source.as_str(),
                                        rollout.monitor_seconds,
                                    )}
                                </span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
        {(!plan.plan.diagnostics.is_empty())
            .then(|| {
                view! {
                    <div class="stack-sm">
                        {plan
                            .plan
                            .diagnostics
                            .into_iter()
                            .map(|d| notice(Tone::Warn, format!("{}: {}", d.resource, d.message)))
                            .collect_view()}
                    </div>
                }
            })}
    }
}
