//! The application's immutable releases, newest first.
use super::super::format::timestamp;
use super::super::ui::{
    Icon, Tone, badge, empty, expander_summary, icon, load_older, notice, when,
};
use super::super::{Alive, ClientErrorExt, live_row};
use super::deployments::PromoteAction;
use super::editor;
use crate::application::ApplicationViewExt;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::sync::short_commit;
use piqueld_client::{Client, ReleaseAvailability, ReleaseView, ResolvedSource, ServiceImage};

/// Pages one load fetches at most while looking for the `?release=` one, so a
/// missing or very old release doesn't read the whole history.
const SEARCH_PAGES: usize = 5;

/// Releases of `application`, loaded when shown and on refresh. Older pages
/// are appended on request, and until the `?release=` one is shown, up to
/// `SEARCH_PAGES` per load; a notice then says it is older or missing.
#[component]
pub(super) fn ReleaseHistory(
    /// ID of the application whose releases to list.
    application: String,
) -> impl IntoView {
    let releases = RwSignal::new(Vec::<ReleaseView>::new());
    let cursor = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let application = StoredValue::new(application);
    let requested = StoredValue::new(
        leptos_router::hooks::use_query_map().with_untracked(|q| q.get("release")),
    );
    let alive = Alive::new();
    let load = move |mut next: Option<String>| {
        loading.set(true);
        let (application, requested) = (application.get_value(), requested.get_value());
        let alive = alive.clone();
        spawn_local(async move {
            let mut replace = next.is_none();
            for page_number in 1..=SEARCH_PAGES {
                let result = Client::browser()
                    .releases(&application, next.as_deref())
                    .await;
                if !alive.get() {
                    return;
                }
                match result {
                    Ok(page) => {
                        if std::mem::take(&mut replace) {
                            releases.set(page.items);
                        } else {
                            releases.update(|items| items.extend(page.items));
                        }
                        cursor.set(page.next_cursor.clone());
                        error.set(None);
                        let shown = requested.as_ref().is_none_or(|id| {
                            releases.with_untracked(|items| {
                                items.iter().any(|release| release.id.as_str() == id)
                            })
                        });
                        match page.next_cursor {
                            Some(cursor) if !shown && page_number < SEARCH_PAGES => {
                                next = Some(cursor);
                            }
                            _ => break,
                        }
                    }
                    Err(e) => {
                        error.set(Some(e.user_message()));
                        break;
                    }
                }
            }
            loading.set(false);
        });
    };
    load(None);
    let refresh = load.clone();
    view! {
        <section class="stack-sm" aria-label="Releases">
            <div class="toolbar">
                <p class="hint">
                    "Each deployment whose images are prepared records an immutable release: its manifest and the exact images it prepared. Environments that prepared the same content share one, and releases outlive the environments that recorded them. Image cleanup keeps what environments run and their last few successful deployments; an older release whose built images were removed can't be deployed again."
                </p>
                <div class="toolbar-end">
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| refresh(None)}
                    >
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </div>
            </div>
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            {move || missing_release(requested.get_value(), releases, cursor, loading, error)}
            <Show when={move || {
                !loading.get() && releases.with(Vec::is_empty)
            }}>{empty("No releases yet. Deploying an environment records one.")}</Show>
            <For
                each={move || releases.get()}
                key={|release| release.id.clone()}
                children={move |initial| {
                    let release = live_row(releases, initial, |release| &release.id);
                    view! { <ReleaseCard release={release} /> }
                }}
            />
            {load_older(
                "Load older releases",
                cursor,
                loading,
                move || load(cursor.get_untracked()),
            )}
        </section>
    }
}

/// A notice saying where the `requested` release is once a load stopped
/// without listing it: older than the loaded `releases`, or not one of them.
fn missing_release(
    requested: Option<String>,
    releases: RwSignal<Vec<ReleaseView>>,
    cursor: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
) -> Option<AnyView> {
    let id = requested?;
    let listed = releases.with(|items| items.iter().any(|release| release.id.as_str() == id));
    if listed || loading.get() || error.with(Option::is_some) {
        return None;
    }
    let message = if cursor.with(Option::is_some) {
        format!(
            "Release {id} isn't among the {} newest releases. Load older releases to keep looking.",
            releases.with(Vec::len)
        )
    } else {
        format!("Release {id} isn't one of this application's releases.")
    };
    Some(notice(Tone::Warn, message))
}

/// Expandable release: where its manifest came from, each service's image,
/// provenance, and build inputs, and the deployments that received it by
/// promotion or by ID. Offers to deploy it into each promoted environment,
/// unless it is `planned`: shown in such a deployment's plan, where it starts
/// expanded. Also starts expanded when it matches `?release=`. Its image
/// availability and deployments follow `release`, which a refresh updates;
/// the rest of a release never changes.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn ReleaseCard(
    /// The release to show.
    release: Signal<ReleaseView>,
    /// Whether the release is shown in a deployment's plan.
    #[prop(optional)]
    planned: bool,
) -> impl IntoView {
    let context = editor();
    let initial = release.get_untracked();
    let selected = leptos_router::hooks::use_query_map()
        .with(|q| q.get("release").is_some_and(|id| id == initial.id.as_str()));
    let opened = RwSignal::new(selected || planned);
    let id = initial.id.to_string();
    let targets = move || context.saved.with(ApplicationViewExt::release_targets);
    let manifest_commit = initial.release.commit().map(str::to_owned);
    let images = move || release.with(|release| release.availability.as_ref().map(availability));
    let services = release_services(&initial);
    view! {
        <article class="expander" class:selected={selected}>
            {expander_summary(
                opened,
                view! {
                    <strong>
                        <code>{id.clone()}</code>
                    </strong>
                    <span class="tag">
                        {manifest_commit
                            .as_deref()
                            .map_or_else(
                                || "Saved manifest".to_owned(),
                                |hash| format!("Commit {}", short_commit(hash)),
                            )}
                    </span>
                    {move || images().map(|(tone, label, _)| badge(tone, label))}
                    <span class="meta">{when(initial.created_at_ms)}</span>
                },
            )}
            <div class="expander-body" hidden={move || !opened.get()}>
                <dl class="kv">
                    <dt>"Release ID"</dt>
                    <dd>
                        <code>{id.clone()}</code>
                    </dd>
                    <dt>"Recorded"</dt>
                    <dd>{timestamp(initial.created_at_ms)}</dd>
                    <dt>"Manifest"</dt>
                    <dd>
                        {manifest_commit
                            .map_or_else(
                                || "The saved manifest".to_owned(),
                                |hash| format!("Read at commit {hash}"),
                            )}
                    </dd>
                    <dt>"Content hash"</dt>
                    <dd>
                        <code>{initial.content_hash.to_string()}</code>
                    </dd>
                    {move || {
                        images()
                            .map(|(_, _, detail)| {
                                view! {
                                    <dt>"Images"</dt>
                                    <dd>{detail}</dd>
                                }
                            })
                    }}
                </dl>
                {services}
                {(!planned)
                    .then(|| {
                        view! {
                            <div class="btn-group">
                                <For
                                    each={targets}
                                    key={String::clone}
                                    children={move |environment| {
                                        view! {
                                            <PromoteAction
                                                environment={environment}
                                                release={id.clone()}
                                            />
                                        }
                                    }}
                                />
                            </div>
                        }
                    })}
                <Show when={move || release.with(|release| !release.promotions.is_empty())}>
                    <div class="snapshot-service">
                        <h4>"Deployed to"</h4>
                        <ul class="stack-sm">
                            {move || {
                                release
                                    .with(|release| release.promotions.clone())
                                    .into_iter()
                                    .map(|promotion| {
                                        let environment = promotion.environment_id.to_string();
                                        let href = context
                                            .deployment_href(&environment, &promotion.deployment_id);
                                        view! {
                                            <li class="btn-group">
                                                <A href={href}>{context.environment_name(&environment)}</A>
                                                <span>{context.describe_origin(&promotion.origin)}</span>
                                                <span class="meta">{when(promotion.created_at_ms)}</span>
                                            </li>
                                        }
                                    })
                                    .collect_view()
                            }}
                        </ul>
                    </div>
                </Show>
            </div>
        </article>
    }
}

/// Each service's image in a release, with its provenance and build inputs.
fn release_services(release: &ReleaseView) -> AnyView {
    release
        .release
        .sources()
        .iter()
        .map(|(service, source)| {
            let inputs = release
                .fingerprint
                .services()
                .get(service)
                .map_or_default(|inputs| inputs.fields().clone());
            let provenance = match source {
                ResolvedSource::Image { requested, .. } => format!("Pulled {requested}"),
                ResolvedSource::Git {
                    requested, commit, ..
                } => format!("Built from {} at {commit}", requested.repository),
            };
            view! {
                <div class="snapshot-service">
                    <h4>{service.to_string()}</h4>
                    <dl class="kv">
                        <dt>"Image"</dt>
                        <dd>
                            <code>{source.digest_reference().to_owned()}</code>
                        </dd>
                        <dt>"Provenance"</dt>
                        <dd>{provenance}</dd>
                        {inputs
                            .into_iter()
                            .map(|(field, value)| {
                                view! {
                                    <dt>
                                        <code>{field}</code>
                                    </dt>
                                    <dd>{value}</dd>
                                }
                            })
                            .collect_view()}
                    </dl>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

/// Badge tone and label, and a sentence, saying whether a release's images
/// are still present.
fn availability(availability: &ReleaseAvailability) -> (Tone, &'static str, String) {
    let services = |missing: &[ServiceImage]| {
        missing
            .iter()
            .map(|image| image.service.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match availability {
        ReleaseAvailability::Present => {
            (Tone::Ok, "Images present", "Every image is present.".into())
        }
        ReleaseAvailability::Pullable { missing } => (
            Tone::Warn,
            "Images to pull",
            format!(
                "Missing for {}; deploying the release pulls them again by digest.",
                services(missing)
            ),
        ),
        ReleaseAvailability::Unavailable { missing } => (
            Tone::Bad,
            "Images removed",
            format!(
                "Missing for {}; the release can't be deployed again.",
                services(missing)
            ),
        ),
    }
}
