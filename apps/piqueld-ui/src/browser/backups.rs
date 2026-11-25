//! `/backups` page: the repositories piqueld writes its backups to, their
//! latest checks, and the recovery key of one just created.
use super::auth::confirmed;
use super::observability::{Outcome, Refresh};
use super::ui::{
    Icon, Modal, PageHeader, Tone, badge, empty, icon, loaded, notice, one_time_secret, text_field,
    when,
};
use super::{UserError, access::can, confirm};
use leptos::prelude::*;
use piqueld_client::{
    Client,
    access::{GlobalPermission, Permission},
    backup::{
        BackupRepository, BackupRepositoryName, RecoveryKit, RepositoryLocation, RepositoryPath,
        RepositoryRequest,
    },
};

/// Where a repository is, as its table cell shows it.
fn location(location: &RepositoryLocation) -> String {
    match location {
        RepositoryLocation::Local { path } => path.to_string(),
    }
}

/// `/backups` page: repositories, with buttons to create, check and remove
/// them for accounts with `backups:manage`.
#[component]
pub(super) fn BackupsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let manage = can(Permission::Global(GlobalPermission::BackupsManage));
    let outcome = Outcome::new();
    let opened = RwSignal::new(false);
    let recovery = RwSignal::new(None::<RecoveryKit>);
    let recovery_key = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if recovery_key.get().is_none() {
            recovery.set(None);
        }
    });
    let data = LocalResource::new(move || {
        refresh.track();
        async move { Client::browser().backup_repositories().await.user_error() }
    });
    let created = move |kit: RecoveryKit| {
        recovery_key.set(Some(kit.recovery_key.clone()));
        let name = kit.repository.name.clone();
        recovery.set(Some(kit));
        format!("Created {name}.")
    };
    view! {
        <PageHeader
            title="Backups"
            description="Where piqueld writes its backups, in restic's format. Each repository opens with piqueld's key, or with the recovery key shown once when it is created."
        >
            <button type="button" class="btn" on:click={move |_| refresh.request()}>
                {icon(Icon::Refresh)}
                "Refresh"
            </button>
            <Show when={move || manage.get()}>
                <button
                    type="button"
                    class="btn btn-primary"
                    on:click={move |_| opened.set(true)}
                >
                    {icon(Icon::Plus)}
                    "Create repository"
                </button>
            </Show>
        </PageHeader>
        <div class="stack">
            {move || outcome.message.get().map(|(tone, message)| notice(tone, message))}
            {move || recovery.get().map(|kit| view! { <RecoveryKey kit={kit} key={recovery_key} /> })}
            <section class="card">
                <header>
                    <div>
                        <h3>"Repositories"</h3>
                        <p>
                            "A check opens a repository with piqueld's key and checks that its index, packs and snapshots agree, without reading their data."
                        </p>
                    </div>
                </header>
                {loaded(
                    data,
                    "Loading repositories…",
                    move |repositories| {
                        if repositories.is_empty() {
                            return empty(
                                "No backup repositories. Create one in a directory on this host to keep backups in it.",
                            );
                        }
                        view! {
                            <div class="table-wrap">
                                <table class="table">
                                    <thead>
                                        <tr>
                                            <th>"Name"</th>
                                            <th>"Directory"</th>
                                            <th>"Created"</th>
                                            <th>"Last check"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {repositories
                                            .into_iter()
                                            .map(|repository| view! {
                                                <RepositoryRow
                                                    repository={repository}
                                                    manage={manage}
                                                    outcome={outcome}
                                                    refresh={refresh}
                                                />
                                            })
                                            .collect_view()}
                                    </tbody>
                                </table>
                            </div>
                        }
                            .into_any()
                    },
                )}
            </section>
        </div>
        <CreateRepository opened={opened} outcome={outcome} refresh={refresh} created={created} />
    }
}

/// One repository, its latest check, and its actions.
#[component]
fn RepositoryRow(
    repository: BackupRepository,
    manage: Signal<bool>,
    outcome: Outcome,
    refresh: Refresh,
) -> impl IntoView {
    let name = repository.name.to_string();
    let check = match &repository.last_check {
        None => badge(Tone::Neutral, "never"),
        Some(check) => {
            let (tone, label) = match check.problem {
                None => (Tone::Ok, "passed"),
                Some(_) => (Tone::Bad, "failed"),
            };
            view! {
                <div class="stack-sm">
                    <span>{badge(tone, label)} " " {when(check.checked_at_ms)}</span>
                    {check.problem.clone().map(|problem| view! { <small>{problem}</small> })}
                </div>
            }
            .into_any()
        }
    };
    let (checked, removed) = (name.clone(), name.clone());
    view! {
        <tr>
            <td>
                <strong>{name}</strong>
            </td>
            <td>
                <code>{location(&repository.location)}</code>
            </td>
            <td>{when(repository.created_at_ms)}</td>
            <td>{check}</td>
            <td class="actions">
                <Show when={move || manage.get()}>
                    <span class="btn-group btn-group-end">
                        <button
                            type="button"
                            class="btn btn-sm"
                            disabled={move || outcome.busy.get()}
                            on:click={
                                let name = checked.clone();
                                move |_| {
                                    let name = name.clone();
                                    outcome.run(
                                        async move {
                                            Client::browser()
                                                .check_backup_repository(&name)
                                                .await
                                                .user_error()
                                        },
                                        |repository| match repository
                                            .last_check
                                            .and_then(|check| check.problem)
                                        {
                                            None => format!("{} passed its check.", repository.name),
                                            Some(problem) => {
                                                format!("{} failed its check: {problem}", repository.name)
                                            }
                                        },
                                        refresh,
                                    );
                                }
                            }
                        >
                            "Check"
                        </button>
                        <button
                            type="button"
                            class="btn btn-ghost btn-sm"
                            disabled={move || outcome.busy.get()}
                            on:click={
                                let name = removed.clone();
                                move |_| {
                                    if !confirm(
                                        &format!(
                                            "Remove {name} from piqueld? Its backups stay in the directory, and its recovery key still opens them.",
                                        ),
                                    ) {
                                        return;
                                    }
                                    let name = name.clone();
                                    outcome.run(
                                        async move {
                                            Client::browser()
                                                .delete_backup_repository(&name)
                                                .await
                                                .user_error()
                                        },
                                        |()| "Repository removed; its backups were kept.".into(),
                                        refresh,
                                    );
                                }
                            }
                        >
                            "Remove"
                        </button>
                    </span>
                </Show>
            </td>
        </tr>
    }
}

/// The dialog creating a repository in a directory on the daemon's host.
/// Creating one asks for a passkey confirmation when the sign-in isn't
/// recent, since the response shows the recovery key.
#[component]
fn CreateRepository(
    opened: RwSignal<bool>,
    outcome: Outcome,
    refresh: Refresh,
    created: impl Fn(RecoveryKit) -> String + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let path = RwSignal::new(String::new());
    let invalid = RwSignal::new(None::<String>);
    let create = move || {
        let parsed = BackupRepositoryName::parse(name.get_untracked())
            .map_err(|error| error.to_string())
            .and_then(|name| {
                RepositoryPath::parse(path.get_untracked().trim())
                    .map(|path| (name, path))
                    .map_err(|error| error.to_string())
            });
        let (name, path) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => return invalid.set(Some(message)),
        };
        invalid.set(None);
        opened.set(false);
        let request = RepositoryRequest {
            location: RepositoryLocation::Local { path },
        };
        outcome.run(
            async move {
                let client = Client::browser();
                confirmed(|| client.create_backup_repository(name.as_str(), &request))
                    .await
                    .user_error()
            },
            created,
            refresh,
        );
    };
    view! {
        <Modal title="Create backup repository" opened={opened}>
            {text_field("Name", name)}
            {text_field("Directory on the daemon's host", path)}
            <p class="muted">
                "An absolute path, missing or empty, that the daemon's user can write. A directory on another disk survives this one failing."
            </p>
            {move || invalid.get().map(|message| notice(Tone::Bad, message))}
            <div class="form-actions">
                <button
                    type="button"
                    class="btn btn-primary"
                    disabled={move || outcome.busy.get()}
                    on:click={move |_| create()}
                >
                    "Create repository"
                </button>
            </div>
        </Modal>
    }
}

/// A new repository's recovery key, shown until dismissed and never again.
#[component]
fn RecoveryKey(kit: RecoveryKit, key: RwSignal<Option<String>>) -> impl IntoView {
    let directory = location(&kit.repository.location);
    view! {
        <section class="card" data-tone={Tone::Warn.attr()}>
            <header>
                <div>
                    <h3>{format!("Save the recovery key of {}", kit.repository.name)}</h3>
                    <p>
                        "With this key and the directory "
                        <code>{directory}</code>
                        ", restic opens the repository without piqueld. piqueld never shows the key again: keep both somewhere other than this host."
                    </p>
                </div>
            </header>
            {one_time_secret("Copy recovery key", key)}
        </section>
    }
}
