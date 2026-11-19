//! The configuration a deployment captured, as its Snapshot tab shows it.
use super::variable_values;
use crate::browser::ui::{Tone, notice};
use crate::overview::build_arguments;
use leptos::prelude::*;
use piqueld_client::{
    Build, DeploymentView, HealthCheck, Service, Source, Template, ValidatedRollout,
};

/// The branch, warnings, variable values, services, volumes, and jobs captured
/// in a deployment's configuration snapshot: the rendered configuration, or
/// the captured manifest until a repository-backed manifest is fetched.
#[component]
pub(super) fn DeploymentSnapshot(deployment: Signal<DeploymentView>) -> impl IntoView {
    view! {
        {move || {
            let deployment = deployment.get();
            let (hint, spec) = match &deployment.application {
                Some(application) => {
                    (
                        "Configuration captured when this deployment was accepted, rendered for its environment.",
                        application.to_manifest().spec,
                    )
                }
                None => {
                    (
                        "The repository manifest has not been fetched yet; showing the captured configuration with references unresolved.",
                        deployment.template.to_manifest().spec,
                    )
                }
            };
            let fetched = deployment
                .template
                .spec()
                .manifest
                .as_ref()
                .map(|manifest| {
                    let revision = manifest
                        .repository
                        .commit
                        .as_ref()
                        .unwrap_or(&manifest.repository.branch);
                    format!("Manifest {} from {revision}.", manifest.path)
                });
            view! {
                <p class="hint">{hint}</p>
                {fetched.map(|fetched| view! { <p class="hint">{fetched}</p> })}
                {deployment
                    .warnings
                    .iter()
                    .map(|warning| notice(
                        Tone::Warn,
                        format!("{}: {}", warning.code, warning.message),
                    ))
                    .collect_view()}
                {(!deployment.variables.is_empty())
                    .then(|| {
                        view! {
                            <div class="snapshot-service">
                                <h4>"Variables"</h4>
                                {variable_values(&deployment.variables)}
                            </div>
                        }
                    })}
                {spec
                    .services
                    .into_iter()
                    .map(|service| view! { <SnapshotService service={service} /> })
                    .collect_view()}
                <div class="snapshot-service">
                    <h4>"Volumes"</h4>
                    <p class="hint">
                        {if spec.volumes.is_empty() {
                            "None".to_owned()
                        } else {
                            spec.volumes.into_iter().map(|v| v.name).collect::<Vec<_>>().join(", ")
                        }}
                    </p>
                </div>
                <div class="snapshot-service">
                    <h4>"Jobs"</h4>
                    {if spec.jobs.is_empty() {
                        view! { <p class="hint">"None"</p> }.into_any()
                    } else {
                        view! {
                            <ol class="hint">
                                {spec
                                    .jobs
                                    .into_iter()
                                    .map(|job| {
                                        view! {
                                            <li>
                                                <strong>{job.name}</strong>
                                                {format!(
                                                    " on {}, up to {}s: ",
                                                    job.service,
                                                    job.timeout_seconds,
                                                )}
                                                <code>{join(&job.command, " ")}</code>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ol>
                        }
                            .into_any()
                    }}
                </div>
            }
        }}
    }
}

/// Definition list describing one snapshotted service's configuration.
#[component]
fn SnapshotService(service: Service) -> impl IntoView {
    let source = match service.source.clone() {
        Source::Image { image } => image.to_string(),
        Source::Git {
            repository,
            build:
                Build::Docker {
                    dockerfile,
                    context,
                    args,
                    target,
                },
        } => [
            Some(format!("Git: {repository}")),
            Some(format!("Dockerfile: {dockerfile}")),
            Some(format!("context: {context}")),
            target.map(|target| format!("target: {target}")),
            (!args.is_empty()).then(|| format!("args: {}", build_arguments(&args))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
    };
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "None".to_owned()
        } else {
            items.join("\n")
        }
    };
    view! {
        <div class="snapshot-service">
            <h4>{service.name.clone()}</h4>
            <dl class="kv">
                <dt>"Source"</dt>
                <dd>
                    <code>{source}</code>
                </dd>
                <dt>"Replicas"</dt>
                <dd>{service.replicas.to_string()}</dd>
                <dt>"Environment"</dt>
                <dd>
                    {list(service.environment.iter().map(|(k, v)| format!("{k}={v}")).collect())}
                </dd>
                <dt>"Command"</dt>
                <dd>{list(service.command.iter().map(ToString::to_string).collect())}</dd>
                <dt>"Arguments"</dt>
                <dd>{list(service.arguments.iter().map(ToString::to_string).collect())}</dd>
                <dt>"Mounts"</dt>
                <dd>
                    {list(
                        service
                            .mounts
                            .iter()
                            .map(|m| {
                                format!(
                                    "{} → {}{}",
                                    m.volume,
                                    m.target,
                                    if m.read_only { " (read only)" } else { "" },
                                )
                            })
                            .collect(),
                    )}
                </dd>
                <dt>"Secret files"</dt>
                <dd>
                    {list(
                        service
                            .secrets
                            .iter()
                            .map(|s| format!("{} → {}", s.name, s.target))
                            .collect(),
                    )}
                </dd>
                <dt>"Startup dependencies"</dt>
                <dd>{list(service.depends_on.clone())}</dd>
                <dt>"Rollout"</dt>
                <dd>
                    {format!(
                        "{} · monitor {}s",
                        service
                            .rollout
                            .order
                            .as_ref()
                            .map_or_else(|| "derived from mounts".into(), ToString::to_string),
                        service
                            .rollout
                            .monitor_seconds
                            .as_ref()
                            .map_or_else(
                                || ValidatedRollout::DEFAULT_MONITOR_SECONDS.to_string(),
                                ToString::to_string,
                            ),
                    )}
                </dd>
                <SnapshotRuntime service={service} />
            </dl>
        </div>
    }
}

/// Health check and resource limit rows for `SnapshotService`.
#[component]
fn SnapshotRuntime(service: Service) -> impl IntoView {
    view! {
        <dt>"Health check"</dt>
        <dd>
            {service
                .healthcheck
                .map_or_else(
                    || "None".into(),
                    |check| {
                        let probe = match &check {
                            HealthCheck::Http { port, path, .. } => format!("HTTP :{port}{path}"),
                            HealthCheck::Command { command, .. } => join(command, " "),
                        };
                        let timing = check.timing();
                        format!(
                            "{probe} · every {}s · timeout {}s · start period {}s · {} retries",
                            timing.interval_seconds,
                            timing.timeout_seconds,
                            timing.start_period_seconds,
                            timing.retries,
                        )
                    },
                )}
        </dd>
        <dt>"Resource limits"</dt>
        <dd>
            {service
                .resources
                .map_or_else(
                    || "Runtime defaults".into(),
                    |r| {
                        format!(
                            "CPU: {} · Memory: {}",
                            r
                                .cpu_millis
                                .map_or_else(|| "default".into(), |v| format!("{v} millicores")),
                            r
                                .memory_bytes
                                .map_or_else(|| "default".into(), |v| format!("{v} bytes")),
                        )
                    },
                )}
        </dd>
    }
}

/// Joins template elements as written.
fn join(values: &[Template], separator: &str) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(separator)
}
