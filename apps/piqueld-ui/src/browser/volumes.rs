//! Detached volumes, which deleted environments leave, on the daemon status
//! page: what each held and where it came from, with a button to delete it.
use super::observability::{Outcome, Refresh};
use super::ui::{empty, loaded, notice, when};
use super::{UserError, access::can, confirm};
use leptos::prelude::*;
use piqueld_client::{
    Client,
    access::{GlobalPermission, Permission},
    volumes::VolumeView,
};

/// The detached volumes, with a Delete button for accounts with
/// `system:operate`. Attaching one is a CLI command, since it names the
/// environment and volume name to bind.
#[component]
pub(super) fn DetachedVolumes(refresh: Refresh) -> impl IntoView {
    let operate = can(Permission::Global(GlobalPermission::SystemOperate));
    let outcome = Outcome::new();
    let data = LocalResource::new(move || {
        refresh.track();
        async move { Client::browser().volumes(true).await.user_error() }
    });
    let row = move |volume: VolumeView| {
        let id = volume.id.to_string();
        view! {
            <tr>
                <td>
                    <code>{id.clone()}</code>
                </td>
                <td>{volume.whereabouts()}</td>
                <td class="num">{volume.size()}</td>
                <td class="muted">{when(volume.created_at_ms)}</td>
                <td>
                    <Show when={move || operate.get()}>
                        <button
                            type="button"
                            class="btn btn-ghost btn-sm"
                            disabled={move || outcome.busy.get()}
                            on:click={
                                let id = id.clone();
                                move |_| {
                                    if !confirm(
                                        &format!(
                                            "Delete volume {id} and all its data? This can't be undone.",
                                        ),
                                    ) {
                                        return;
                                    }
                                    let id = id.clone();
                                    outcome.run(
                                        async move { Client::browser().delete_volume(&id).await },
                                        |()| "Volume deleted.".into(),
                                        refresh,
                                    );
                                }
                            }
                        >
                            "Delete"
                        </button>
                    </Show>
                </td>
            </tr>
        }
    };
    view! {
        <section class="card card-flush" aria-labelledby="detached-volumes-heading">
            <header>
                <div>
                    <h3 id="detached-volumes-heading">"Detached volumes"</h3>
                    <p>
                        "Volumes no environment binds, such as those of deleted environments, kept with their data. Attach one to an environment with "
                        <code>"piquelctl volume attach"</code>
                        ", or delete it to free its space."
                    </p>
                </div>
            </header>
            {move || outcome.message.get().map(|(tone, message)| notice(tone, message))}
            {loaded(
                data,
                "Loading detached volumes…",
                move |volumes| {
                    if volumes.is_empty() {
                        return empty("No detached volumes. Deleting an environment keeps its volumes here.");
                    }
                    view! {
                        <table class="table">
                            <thead>
                                <tr>
                                    <th>"Volume"</th>
                                    <th>"Came from"</th>
                                    <th class="num">"Size"</th>
                                    <th>"Created"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>{volumes.into_iter().map(row).collect_view()}</tbody>
                        </table>
                    }
                        .into_any()
                },
            )}
        </section>
    }
}
