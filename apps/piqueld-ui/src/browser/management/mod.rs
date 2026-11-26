//! Application editor state, shared by every page of one application, and
//! the components its routes mount. Polling never replaces local edits.
mod deployments;
mod environments;
mod forms;
mod jobs;
mod links;
mod logs;
mod navigation;
mod pages;
mod previews;
mod releases;
mod routes;
mod secrets;
mod services;
mod settings;
mod terminal;
mod timeline;
mod traffic;
mod variables;

use super::ui::{Icon, Modal, TabSet, Tone, icon, loading, notice, text_field};
use super::{
    Alive, ClientErrorExt, DashboardContext, DashboardSignals,
    access::{self, can},
    dashboard_context, environment_href,
};
use crate::application::ApplicationViewExt;
pub(super) use crate::application::Page;

use forms::{Draft, RowDialog, Rows};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::{A, ToHref};
use leptos_router::hooks::use_navigate;
use leptos_router::params::ParamsMap;
use navigation::guard_navigation;
pub(super) use navigation::{HistoryGuard, remember_tab};
use pages::{ApplicationSections, EnvironmentPage};
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, ApplicationTemplate, ApplicationView,
    ApplyApplicationRequest, Client, ClientError, DeploymentOrigin, EnvironmentConfig,
    EnvironmentSource, EnvironmentView, ManifestRevision, Metadata,
    access::{AppPermission, GlobalPermission, Grants, Permission},
    edit::{ApplicationEdit, EditOptions},
};
use std::collections::BTreeSet;

impl Page {
    /// The page an `/applications/:id[/services/:service|/environments/:environment|/previews/:preview]`
    /// route shows, or with `/deployments/:deployment`, one of the
    /// environment's or preview's deployments.
    pub(super) fn from_params(params: &ParamsMap) -> Self {
        let environment = params.get("environment").or_else(|| params.get("preview"));
        if let Some(service) = params.get("service") {
            Self::Service(service)
        } else if let (Some(environment), Some(operation)) = (environment, params.get("deployment"))
        {
            Self::Deployment {
                environment,
                operation,
            }
        } else if let Some(environment) = params.get("environment") {
            Self::Environment(environment)
        } else if let Some(preview) = params.get("preview") {
            Self::Preview(preview)
        } else {
            Self::Application
        }
    }
}

/// State shared by every section of one application editor, provided as context.
#[derive(Clone, Copy)]
struct EditorContext {
    /// The dashboard's context.
    dashboard: StoredValue<DashboardContext>,
    /// The signed-in credential's grants, which `can` checks.
    grants: Signal<Grants>,
    /// The application as last saved; edits are applied here only once the daemon accepts them.
    saved: RwSignal<ApplicationView>,
    /// Which page of the application the editor shows.
    page: StoredValue<Page>,
    /// Environment that runtime actions target: the environment page's, or on
    /// application pages the only environment. `None` with several or none.
    environment: Memo<Option<String>>,
    /// Keys of the forms with unsaved edits.
    dirty: RwSignal<BTreeSet<String>>,
    /// Whether a request is in flight.
    busy: RwSignal<bool>,
    /// Whether the last request's outcome is unknown, after a transport failure.
    uncertain: RwSignal<bool>,
    /// The error shown to the user; `None` when there is none.
    error: RwSignal<Option<String>>,
    /// ID of the diagnostic event behind `error`, for a link to it.
    diagnostic_id: RwSignal<Option<String>>,
    /// The success message shown to the user; empty when there is none.
    notice: RwSignal<String>,
    /// The page's tabs; `tabs.selected` is the shown one.
    tabs: TabSet,
}

impl EditorContext {
    /// The dashboard's shared signals.
    fn signals(self) -> DashboardSignals {
        self.dashboard.with_value(|dashboard| dashboard.signals)
    }

    /// Refreshes the dashboard, so lists and the runtime detail catch up with
    /// a change.
    fn refresh_dashboard(self) {
        self.dashboard
            .with_value(|dashboard| dashboard.refresh.run(()));
    }

    /// The saved manifest, tracked so lists re-render after every save.
    fn manifest(self) -> ApplicationManifest {
        self.saved.with(|saved| saved.application.to_manifest())
    }

    /// The manifest the shown environment deploys: the saved one, or the one
    /// last fetched from its branch or promoted into it, read from its loaded
    /// detail. `None` before a branch's first fetch or an environment's first
    /// promotion, or while the detail loads.
    fn environment_manifest(self) -> Option<ApplicationTemplate> {
        let environment = self.selected_environment()?;
        if environment.source == EnvironmentSource::Saved {
            return Some(self.saved.with(|saved| saved.application.clone()));
        }
        self.signals().detail.with(|detail| {
            let detail = detail
                .as_ref()
                .filter(|detail| detail.environment.id == environment.id)?;
            detail.manifest.clone()
        })
    }

    /// The manifest the shown environment's next deployment deploys, as far
    /// as known without fetching: the saved one for an environment following
    /// the branch the connection names, once the application read a manifest
    /// from it, which it deploys until a push changes the manifest there;
    /// otherwise its own (see `environment_manifest`).
    fn next_manifest(self) -> Option<ApplicationTemplate> {
        let environment = self.selected_environment()?;
        self.saved
            .with(|saved| {
                saved
                    .follows_connection(&environment)
                    .then(|| saved.application.clone())
            })
            .or_else(|| self.environment_manifest())
    }

    /// Whether the shown environment's next deployment builds a service
    /// from Git, as far as its manifest is known (see `next_manifest`).
    fn builds_from_git(self) -> bool {
        self.next_manifest().is_some_and(|manifest| {
            manifest
                .to_manifest()
                .spec
                .services
                .iter()
                .any(|service| matches!(service.source, piqueld_client::Source::Git { .. }))
        })
    }

    /// Service names of the manifest the shown environment deploys, or of
    /// the saved one while that is unknown.
    fn environment_services(self) -> Vec<String> {
        self.environment_manifest()
            .map_or_else(|| self.manifest(), |manifest| manifest.to_manifest())
            .spec
            .services
            .into_iter()
            .map(|service| service.name)
            .collect()
    }

    /// The application's ID.
    fn id(self) -> String {
        self.saved
            .with_untracked(|saved| saved.application.id().to_string())
    }

    /// The shown environment's ID, read once by components keyed on it.
    fn environment_id(self) -> String {
        self.environment.get_untracked().unwrap_or_default()
    }

    /// The environment runtime actions target, if it still exists.
    fn selected_environment(self) -> Option<EnvironmentView> {
        let id = self.environment.get()?;
        self.saved.with(|saved| saved.deployable(&id).cloned())
    }

    /// Name of the environment runtime actions target; empty without one.
    fn selected_name(self) -> String {
        self.selected_environment()
            .map_or_default(|environment| environment.name.to_string())
    }

    /// Whether the environment runtime actions target is being deleted.
    fn selected_deleting(self) -> bool {
        self.selected_environment()
            .is_some_and(|environment| environment.delete_intent)
    }

    /// Runtime actions require a loaded, live environment, including after
    /// deletion. Background refreshes of an already loaded detail keep them
    /// enabled, so a click during a poll is never swallowed.
    fn environment_action_blocked(self) -> bool {
        let signals = self.signals();
        self.action_blocked()
            || self
                .selected_environment()
                .is_none_or(|env| env.delete_intent)
            || signals.detail_error.get().is_some()
            || signals.detail.with(|detail| {
                detail.as_ref().is_none_or(|detail| {
                    Some(detail.environment.id.to_string()) != self.environment.get()
                })
            })
    }

    /// Dashboard address of one of the application's environments.
    fn environment_href(self, environment: &str) -> String {
        environment_href(&self.id(), environment)
    }

    /// Dashboard address of one of the application's previews.
    fn preview_href(self, preview: &str) -> String {
        format!("/dashboard/applications/{}/previews/{preview}", self.id())
    }

    /// Dashboard address of one of the application's environments or
    /// previews.
    fn deployable_href(self, id: &str) -> String {
        let preview = self.saved.with(|saved| {
            saved
                .previews
                .iter()
                .any(|preview| preview.id.as_str() == id)
        });
        if preview {
            self.preview_href(id)
        } else {
            self.environment_href(id)
        }
    }

    /// Dashboard address of deployment `operation` of one of the
    /// application's environments or previews.
    fn deployment_href(self, environment: &str, operation: &str) -> String {
        format!(
            "{}/deployments/{operation}",
            self.deployable_href(environment)
        )
    }

    /// Name of one of the application's environments or previews; its ID
    /// once it is gone.
    fn environment_name(self, environment: &str) -> String {
        self.saved.with(|saved| saved.environment_name(environment))
    }

    /// Whether one of the application's environments or previews is being
    /// deleted, or is already gone.
    fn deleting(self, environment: &str) -> bool {
        self.saved.with(|saved| {
            saved
                .deployable(environment)
                .is_none_or(|environment| environment.delete_intent)
        })
    }

    /// Where an environment deploys from (see `ApplicationViewExt::describe_source`).
    fn describe_source(self, source: &EnvironmentSource) -> String {
        self.saved.with(|saved| saved.describe_source(source))
    }

    /// Where a deployment came from (see `ApplicationViewExt::describe_origin`).
    fn describe_origin(self, origin: &DeploymentOrigin) -> String {
        self.saved.with(|saved| saved.describe_origin(origin))
    }

    /// See `ApplicationViewExt::deployment_title`.
    fn deployment_title(self, origin: &DeploymentOrigin) -> String {
        self.saved.with(|saved| saved.deployment_title(origin))
    }

    /// The application's name.
    fn name(self) -> String {
        self.saved
            .with(|saved| saved.application.metadata().name.to_string())
    }

    /// Whether the saved manifest declares the environment `name`, owning
    /// whether it exists and where it deploys from.
    fn declared(self, name: &str) -> bool {
        self.saved
            .with(|saved| saved.application.spec().environments.contains_key(name))
    }

    /// Whether the saved manifest's block for the environment `name` sets
    /// where it deploys from, owning its source and deploy on push.
    fn source_declared(self, name: &str) -> bool {
        self.saved.with(|saved| {
            saved
                .application
                .spec()
                .environments
                .get(name)
                .is_some_and(EnvironmentConfig::sets_source)
        })
    }

    /// Options choosing one of the saved manifest's services, after a blank
    /// "Select a service".
    fn service_options(self) -> impl IntoView {
        view! {
            <option value="">"Select a service"</option>
            {move || {
                self.manifest()
                    .spec
                    .services
                    .into_iter()
                    .map(|service| {
                        view! {
                            <option value={service.name.clone()}>{service.name.clone()}</option>
                        }
                    })
                    .collect_view()
            }}
        }
    }

    /// Options choosing one of `environment_services`, which follow the
    /// shown environment's manifest.
    fn environment_service_options(self) -> impl IntoView {
        move || {
            self.environment_services()
                .into_iter()
                .map(|name| view! { <option value={name.clone()}>{name.clone()}</option> })
                .collect_view()
        }
    }

    /// Whether the application reads its manifest from a Git repository.
    fn repository_backed(self) -> bool {
        self.saved
            .with(|saved| saved.application.spec().manifest.is_some())
    }

    /// Whether the signed-in credential holds `permission` on the application,
    /// or in `environment`, if that is one of its previews, the previews
    /// permission that also allows it there (see `AppPermission::on_previews`).
    /// Tracked, so controls hidden or disabled by it follow the session; the
    /// daemon stays authoritative.
    fn can_in(self, permission: AppPermission, environment: Option<&str>) -> bool {
        self.saved.with(|saved| {
            self.grants
                .with(|grants| saved.allows(grants, permission, environment))
        })
    }

    /// `can_in` the environment or preview runtime actions target, if any.
    fn can(self, permission: AppPermission) -> bool {
        self.can_in(permission, self.environment.get().as_deref())
    }

    /// Notice that configuration forms are read-only without `apps:write`.
    fn read_only_notice(self) -> Option<AnyView> {
        (!self.can(AppPermission::Write)).then(|| {
            notice(
                Tone::Info,
                "Read-only: editing this configuration requires the apps:write permission.",
            )
        })
    }

    /// Whether the caller may not open tab `tab`: logs and traffic need
    /// `logs:read`, the terminal `apps:exec`, and history `events:read`.
    fn tab_hidden(self, tab: &str) -> bool {
        match tab {
            "Logs" | "Traffic" => !self.can(AppPermission::LogsRead),
            "Terminal" => !self.can(AppPermission::Exec),
            "Events" => !self.can(AppPermission::EventsRead),
            _ => false,
        }
    }

    /// Whether saves are currently disallowed.
    fn blocked(self) -> bool {
        self.busy.get() || self.uncertain.get()
    }

    /// Whether deploy/delete actions are disallowed: also blocked by unsaved edits
    /// or a pending deletion.
    fn action_blocked(self) -> bool {
        self.change_blocked(None)
    }

    /// Whether a change saving the `own` form's edits is disallowed: like
    /// `action_blocked`, but the form's own unsaved edits are what it saves.
    fn change_blocked(self, own: Option<&str>) -> bool {
        self.blocked() || self.other_edits(own) || self.saved.with(|saved| saved.delete_intent)
    }

    /// Whether forms other than `own` have unsaved edits.
    fn other_edits(self, own: Option<&str>) -> bool {
        self.dirty
            .with(|groups| groups.iter().any(|group| Some(group.as_str()) != own))
    }

    /// Keeps the form `key` in the dirty set while `changed` holds, until
    /// the owning view unmounts.
    fn guard(self, key: String, changed: impl Fn() -> bool + 'static) {
        let cleanup = key.clone();
        Effect::new(move |_| {
            let dirty = changed();
            self.dirty.update(|groups| {
                if dirty {
                    groups.insert(key.clone());
                } else {
                    groups.remove(&key);
                }
            });
        });
        on_cleanup(move || {
            self.dirty.update(|groups| {
                groups.remove(&cleanup);
            });
        });
    }

    /// Whether forms have edits or a request is pending or has an unknown
    /// outcome, so the saved view must not be replaced under them.
    fn editing(self) -> bool {
        !self.dirty.get_untracked().is_empty()
            || self.busy.get_untracked()
            || self.uncertain.get_untracked()
    }

    /// Whether a repository-backed application at revision `generation`,
    /// last updated at `updated_at_ms`, holds a manifest fetched after the
    /// saved view's: same revision, updated later.
    fn fetched_since(self, generation: u64, updated_at_ms: i64) -> bool {
        self.saved
            .with_untracked(|saved| saved.fetched_since(generation, updated_at_ms))
    }

    /// Whether the editor is still mounted. Reading its disposed handles
    /// panics, so async work checks this after every await.
    fn mounted(self) -> bool {
        !self.saved.is_disposed()
    }

    /// Whether this is an environment page rather than the application page.
    fn environment_page(self) -> bool {
        self.page
            .with_value(|page| matches!(page, Page::Environment(_)))
    }

    /// Replaces the editor error and clears any diagnostic link.
    fn set_error(self, error: Option<String>) {
        self.diagnostic_id.set(None);
        self.error.set(error);
    }

    /// Shows a request failure. Transport failures mark the editor `uncertain`
    /// because the server may or may not have applied the request.
    fn failure(self, error: &ClientError) {
        if error.transport() {
            self.uncertain.set(true);
            self.set_error(Some("The request outcome is unknown. Reload saved configuration and deployment history before another action.".into()));
        } else {
            self.set_error(Some(error.user_message()));
            self.diagnostic_id.set(error.diagnostic_id());
        }
    }

    /// Applies `edit` to the saved manifest, validates it locally, then sends it
    /// guarded by the saved generation (see `mutate`). On success the saved view is
    /// updated locally, `on_saved` runs, and the dashboard refreshes.
    fn save(self, edit: ApplicationEdit, on_saved: impl FnOnce(ApplicationView) + 'static) {
        if self.blocked() {
            return;
        }
        self.set_error(None);
        let mut manifest = self.manifest();
        if let Err(error) = edit.clone().apply(&mut manifest) {
            self.set_error(Some(error.to_string()));
            return;
        }
        let validated = match manifest.validate_template() {
            Ok(validated) => validated,
            Err(error) => {
                self.set_error(Some(error.to_string()));
                return;
            }
        };
        let saved = self.saved.get_untracked();
        let id = saved.application.id().to_string();
        let options = EditOptions {
            expected_generation: Some(saved.generation),
            ..EditOptions::default()
        };
        self.mutate(
            move |client| {
                let (id, edit) = (id.clone(), edit.clone());
                async move { client.edit_application(&id, &edit, &options).await }
            },
            move |receipt| {
                let application = validated.normalize(saved.application.id().clone());
                let repository = application.spec().manifest.clone().and_then(|connection| {
                    saved
                        .repository
                        .clone()
                        .map(|repository| repository.with_manifest(Some(connection)))
                });
                let updated = ApplicationView {
                    spec_hash: application.spec_hash(),
                    application,
                    generation: receipt.generation,
                    repository,
                    ..saved
                };
                self.saved.set(updated.clone());
                on_saved(updated);
                self.notice.set("Saved.".into());
                self.refresh_dashboard();
            },
        );
    }

    /// Saves the manifest last read from the repository again, after
    /// confirmation, discarding the edits made since; deploying stays
    /// explicit.
    fn reset_edits(self) {
        let saved = self.saved.get_untracked();
        let Some(repository) = saved.repository.clone() else {
            return;
        };
        if self.action_blocked() {
            return;
        }
        let request = ApplyApplicationRequest {
            manifest: repository.to_manifest(),
            expected_generation: Some(saved.generation),
            expected_application_id: Some(saved.application.id().to_string()),
        };
        super::confirm(
            "Reset to repository",
            "Discard every edit made since the manifest was last read from the repository, and save the repository's manifest again? Deploy afterwards to apply it.",
            move || {
                self.mutate(
                    move |client| {
                        let request = request.clone();
                        async move { client.apply_application(&request).await }
                    },
                    move |receipt| {
                        self.saved.set(ApplicationView {
                            spec_hash: repository.spec_hash(),
                            application: repository,
                            generation: receipt.generation,
                            ..saved
                        });
                        self.notice
                            .set("Reset to the repository's manifest.".into());
                        self.refresh_dashboard();
                    },
                );
            },
        );
    }

    /// Shows `environment`'s deployments: the Deployments tab on its own
    /// page, otherwise by navigating there.
    fn show_deployments(self, environment: &str, navigate: impl Fn(&str, NavigateOptions)) {
        if self.environment_page()
            && self.environment.get_untracked().as_deref() == Some(environment)
        {
            self.tabs.selected.set("Deployments");
        } else {
            let href = format!("{}?tab=deployments", self.environment_href(environment));
            navigate(&href, NavigateOptions::default());
        }
    }

    /// Sends `request` while the editor is busy, then runs `done` with its
    /// result, unless the editor unmounted meanwhile.
    fn run<T: 'static>(
        self,
        request: impl Future<Output = Result<T, ClientError>> + 'static,
        done: impl FnOnce(Result<T, ClientError>) + 'static,
    ) {
        self.set_error(None);
        self.busy.set(true);
        spawn_local(async move {
            let result = request.await;
            if !self.mounted() {
                return;
            }
            done(result);
            self.busy.set(false);
        });
    }

    /// Runs one mutation (see `run`): `send` gets a client with a fresh
    /// request identity, then `done` runs with the response, or the failure
    /// is shown.
    fn run_mutation<T: 'static>(
        self,
        send: impl AsyncFnOnce(&Mutation) -> Result<T, ClientError> + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        let mutation = match Mutation::new() {
            Ok(mutation) => mutation,
            Err(error) => {
                self.set_error(Some(error));
                return;
            }
        };
        self.run(
            async move { send(&mutation).await },
            move |result| match result {
                Ok(response) => done(response),
                Err(error) => self.failure(&error),
            },
        );
    }

    /// Reloads the saved configuration in place. Forms without edits show
    /// it; edited ones keep their edits, now compared against it (see
    /// `Draft::synced`). Clears an unknown request outcome and refreshes
    /// deployment history.
    fn reload(self) {
        if self.busy.get_untracked() {
            return;
        }
        let id = self.id();
        self.run(
            async move { Client::browser().application(&id).await },
            move |result| match result {
                Ok(view) => {
                    self.saved.set(view);
                    self.uncertain.set(false);
                    self.notice.set("Reloaded the saved configuration.".into());
                    self.refresh_dashboard();
                }
                Err(error) => self.set_error(Some(error.user_message())),
            },
        );
    }

    /// Sends one mutation (see `run_mutation`), retrying once on transport
    /// failure with the same identity so the server can deduplicate it.
    fn mutate<T: 'static, F: Future<Output = Result<T, ClientError>>>(
        self,
        request: impl Fn(Client) -> F + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        self.run_mutation(async move |mutation| mutation.send(request).await, done);
    }

    /// Sends one mutation (see `run_mutation`) without retrying it:
    /// replaying a deletion or a secret rotation whose outcome is unknown
    /// could repeat it, or fail once the first attempt completed.
    fn mutate_once<T: 'static>(
        self,
        request: impl AsyncFnOnce(Client) -> Result<T, ClientError> + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        self.run_mutation(
            async move |mutation| request(mutation.client().clone()).await,
            done,
        );
    }

    /// Deploys the saved generation to `environment`, then runs `accepted`.
    /// `revision` fetches the repository manifest from another branch or
    /// commit than the environment's, for this deployment only. `rebuild`
    /// builds every image from Git again, even one that could be reused.
    fn deploy(
        self,
        environment: String,
        revision: Option<ManifestRevision>,
        rebuild: bool,
        accepted: impl FnOnce() + 'static,
    ) {
        let generation = self.saved.with_untracked(|saved| saved.generation);
        self.mutate(
            move |client| {
                let (environment, revision) = (environment.clone(), revision.clone());
                async move {
                    client
                        .deploy_environment(&environment, generation, revision.as_ref(), rebuild)
                        .await
                }
            },
            move |_| {
                self.notice
                    .set("Deployment accepted. Follow its progress below.".into());
                accepted();
                self.refresh_dashboard();
            },
        );
    }

    /// Stops or starts `environment`, then marks it so in the saved view.
    /// Stopping withdraws its routes and scales its services to zero;
    /// starting reconciles it, scaling them back up.
    fn set_stopped(self, environment: String, stopped: bool) {
        let id = environment.clone();
        self.mutate(
            move |client| {
                let environment = environment.clone();
                async move {
                    if stopped {
                        client.stop_environment(&environment, None).await
                    } else {
                        client.start_environment(&environment, None).await
                    }
                }
            },
            move |_| {
                self.saved.update(|saved| {
                    if let Some(environment) = saved
                        .environments
                        .iter_mut()
                        .find(|environment| environment.id.as_str() == id)
                    {
                        environment.run_state = piqueld_client::RunState::new(stopped);
                    }
                });
                self.notice.set(if stopped {
                    "Stopping: its routes are withdrawn and its services scale to zero.".into()
                } else {
                    "Starting: its services scale back up and its routes are served again.".into()
                });
                self.refresh_dashboard();
            },
        );
    }
}

/// How long the editor's notice of a completed action stays.
const NOTICE_SHOWN: std::time::Duration = std::time::Duration::from_secs(8);

/// Returns the enclosing `EditorContext`.
#[expect(
    clippy::expect_used,
    reason = "`ApplicationEditor` provides the context before rendering its children."
)]
fn editor() -> EditorContext {
    use_context().expect("ApplicationEditor provides EditorContext before rendering its children")
}

/// A client tagged with a fresh random request ID, used for mutations so a
/// retried request is recognised as a replay rather than applied twice.
struct Mutation(Client);

impl Mutation {
    /// Draws the request identity.
    #[expect(
        clippy::map_err_ignore,
        reason = "a browser failure reaches the page only as this fixed message"
    )]
    fn new() -> Result<Self, String> {
        let error = "Browser could not create a request identity.";
        let mut bytes = [0; 16];
        window()
            .crypto()
            .map_err(|_| error)?
            .get_random_values_with_u8_array(&mut bytes)
            .map_err(|_| error)?;
        let id = hex::encode(bytes);
        Ok(Self(Client::browser().with_request_id(id)))
    }

    /// The tagged client, for a request sent only once.
    fn client(&self) -> &Client {
        &self.0
    }

    /// Sends `request`, and once more after a transport failure, whose
    /// outcome is unknown.
    async fn send<T, F: Future<Output = Result<T, ClientError>>>(
        &self,
        request: impl Fn(Client) -> F,
    ) -> Result<T, ClientError> {
        let result = request(self.0.clone()).await;
        if result.as_ref().is_err_and(ClientErrorExt::transport) {
            return request(self.0.clone()).await;
        }
        result
    }
}

/// "Create application" button and modal. Validates the name locally, creates an
/// empty application (retrying once on transport failure), refreshes the
/// dashboard and navigates to the new application.
#[component]
pub(super) fn CreateApplication() -> impl IntoView {
    let opened = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let alive = Alive::new();
    let create = move || {
        if busy.get_untracked() {
            return;
        }
        let manifest = ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: Metadata {
                name: name.get_untracked(),
            },
            spec: ApplicationSpec::default(),
        };
        if let Err(problem) = manifest.clone().validate() {
            error.set(Some(problem.to_string()));
            return;
        }
        let mutation = match Mutation::new() {
            Ok(mutation) => mutation,
            Err(problem) => {
                error.set(Some(problem));
                return;
            }
        };
        let (navigate, alive) = (navigate.clone(), alive.clone());
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = manifest.metadata.name;
            let result = mutation
                .send(|client| {
                    let name = name.clone();
                    async move { client.create_application(&name, false).await }
                })
                .await;
            if !alive.get() {
                return;
            }
            match result {
                Ok(saved) => {
                    refresh.run(());
                    navigate(
                        &format!("/dashboard/applications/{}", saved.application_id),
                        NavigateOptions::default(),
                    );
                }
                Err(problem) => error.set(Some(problem.user_message())),
            }
            busy.set(false);
        });
    };
    let allowed = can(Permission::Global(GlobalPermission::AppsCreate));
    view! {
        <Show when={move || allowed.get()}>
            <button type="button" class="btn btn-primary" on:click={move |_| opened.set(true)}>
                {icon(Icon::Plus)}
                "New application"
            </button>
        </Show>
        <Modal
            title="Create application"
            opened={opened}
            busy={busy}
            on_close={Callback::new(move |()| {
                name.set(String::new());
                error.set(None);
            })}
        >
            <form
                class="stack-sm"
                on:submit={move |event| {
                    event.prevent_default();
                    create();
                }}
            >
                <fieldset class="stack-sm" disabled={move || busy.get()}>
                    <p class="hint">
                        "The application starts empty. Add services, routes, and volumes afterwards, then deploy."
                    </p>
                    {text_field("Application name", name)}
                </fieldset>
                {move || error.get().map(|e| notice(Tone::Bad, e))}
                <div class="form-actions">
                    <button type="submit" class="btn btn-primary" disabled={move || busy.get()}>
                        "Create application"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// Loads saved application `id` once and mounts `ApplicationEditor` for `page`.
#[component]
pub(super) fn ApplicationPage(
    /// ID of the application to load.
    id: String,
    /// Which page of it to show.
    page: Page,
) -> impl IntoView {
    let initial = RwSignal::new(None::<ApplicationView>);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        match Client::browser().application(&id).await {
            Ok(app) => initial.set(Some(app)),
            Err(e) => error.set(Some(e.user_message())),
        }
    });
    view! {
        {move || match (initial.get(), error.get()) {
            (Some(initial), _) => {
                view! { <ApplicationEditor initial={initial} page={page.clone()} /> }.into_any()
            }
            (None, Some(e)) => {
                view! {
                    <div class="stack-sm">
                        {notice(Tone::Bad, e)}
                        {back_link("/dashboard/applications", "Back to applications")}
                    </div>
                }
                    .into_any()
            }
            (None, None) => loading("Loading the application…"),
        }}
    }
}

/// A button-styled link back to a list, for pages whose subject is gone.
fn back_link(href: impl ToHref + Send + Sync + 'static, label: impl IntoView + 'static) -> AnyView {
    view! {
        <div class="btn-group">
            <A attr:class="btn" href={href}>
                {icon(Icon::ArrowLeft)}
                {label}
            </A>
        </div>
    }
    .into_any()
}

/// Provides `EditorContext`, guards navigation while edits are unsaved, and renders
/// the application page, a service editor, or an environment page. The initial
/// tab comes from the `deployment` or `tab` query parameters.
#[component]
fn ApplicationEditor(
    /// The application as saved, to start editing from.
    initial: ApplicationView,
    /// Which page of it to show.
    page: Page,
) -> impl IntoView {
    let query = leptos_router::hooks::use_query_map();
    let dashboard = dashboard_context();
    let signals = dashboard.signals;
    let saved = RwSignal::new(initial);
    let shown = match &page {
        Page::Environment(environment)
        | Page::Preview(environment)
        | Page::Deployment { environment, .. } => Some(environment.clone()),
        Page::Application | Page::Service(_) => None,
    };
    let context = EditorContext {
        dashboard: StoredValue::new(dashboard),
        grants: access::grants(),
        saved,
        environment: Memo::new(move |_| {
            shown.clone().or_else(|| {
                saved.with(|saved| {
                    saved
                        .sole_environment()
                        .ok()
                        .map(|environment| environment.id.to_string())
                })
            })
        }),
        dirty: RwSignal::new(BTreeSet::new()),
        busy: RwSignal::new(false),
        uncertain: RwSignal::new(false),
        error: RwSignal::new(None),
        diagnostic_id: RwSignal::new(None),
        notice: RwSignal::new(String::new()),
        tabs: TabSet::new(
            page.tabs_label(),
            RwSignal::new(query.with_untracked(|query| page.initial_tab(|key| query.get(key)))),
        ),
        page: StoredValue::new(page.clone()),
    };
    provide_context(context);
    remember_tab(context.tabs.selected);
    Effect::new(move |_| {
        if let Some(application) = signals.applications.with(|rows| {
            rows.iter()
                .find(|row| row.application.id.as_str() == context.id())
                .map(|row| row.application.clone())
        }) {
            let environments = application
                .environments
                .iter()
                .map(|listed| listed.environment.clone())
                .collect::<Vec<_>>();
            if context.saved.with_untracked(|saved| {
                application.generation >= saved.generation
                    && (saved.environments != environments
                        || saved.delete_intent != application.delete_intent)
            }) {
                context.saved.update(|saved| {
                    saved.environments = environments;
                    saved.delete_intent = application.delete_intent;
                });
            }
            if context.fetched_since(application.generation, application.updated_at_ms)
                && !context.editing()
            {
                let id = application.id.to_string();
                spawn_local(async move {
                    if let Ok(view) = Client::browser().application(&id).await
                        && context.mounted()
                        && context.fetched_since(view.generation, view.updated_at_ms)
                        && !context.editing()
                    {
                        context.saved.set(view);
                    }
                });
            }
        }
    });
    Effect::new(move |_| {
        signals.detail.with(|detail| {
            let Some(application) = detail.as_ref().map(|detail| &detail.application) else {
                return;
            };
            if application.application.id().as_str() == context.id()
                && context.saved.with_untracked(|saved| {
                    saved.previews != application.previews
                        || saved.sync_check != application.sync_check
                })
            {
                context.saved.update(|saved| {
                    saved.previews.clone_from(&application.previews);
                    saved.sync_check.clone_from(&application.sync_check);
                });
            }
        });
    });
    guard_navigation(context.dirty);
    match page {
        Page::Service(name) => view! { <services::ServiceEditor name={name} /> }.into_any(),
        Page::Environment(_) | Page::Preview(_) => view! { <EnvironmentPage /> }.into_any(),
        Page::Deployment { operation, .. } => {
            view! { <deployments::DeploymentPage operation={operation} /> }.into_any()
        }
        Page::Application => view! { <ApplicationSections /> }.into_any(),
    }
}

/// Save status, error with diagnostic link, and a conflict notice when the
/// polled listing shows a newer generation than the editor's. Both offer to
/// reload the saved configuration in place, keeping form edits.
#[component]
fn EditorFeedback() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
    Effect::new(move |_| {
        let shown = context.notice.get();
        if shown.is_empty() {
            return;
        }
        set_timeout(
            move || {
                if context.notice.try_get_untracked().as_ref() == Some(&shown) {
                    context.notice.set(String::new());
                }
            },
            NOTICE_SHOWN,
        );
    });
    let show_status = move || {
        context.busy.get() || (context.dirty.get().is_empty() && !context.notice.get().is_empty())
    };
    let reload_button = move || {
        view! {
            <button
                type="button"
                class="btn btn-sm"
                disabled={move || context.busy.get()}
                on:click={move |_| context.reload()}
            >
                "Reload saved configuration"
            </button>
        }
    };
    let conflict = move || {
        signals.applications.with(|rows| {
            rows.iter().any(|row| {
                row.application.id.as_str() == context.id()
                    && row.application.generation > context.saved.get().generation
            })
        })
    };
    view! {
        <div class="stack-sm mb-md">
            <p class="hint" role="status" hidden={move || !show_status()}>
                {move || {
                    if context.busy.get() { "Saving…".into() } else { context.notice.get() }
                }}
            </p>
            {move || {
                context
                    .error
                    .get()
                    .map(|e| {
                        notice(
                            Tone::Bad,
                            view! {
                                <span>{e}</span>
                                <span>
                                    "Your form edits have been kept. Reload to review the latest saved configuration; edits stay until you save or discard them."
                                </span>
                                <div class="btn-group">
                                    {reload_button()}
                                    {context
                                        .diagnostic_id
                                        .get()
                                        .filter(|_| context.can(AppPermission::EventsRead))
                                        .map(|id| {
                                            view! {
                                                <A
                                                    attr:class="btn btn-sm btn-ghost"
                                                    href={format!("/dashboard/errors/{id}")}
                                                >
                                                    "Diagnostic details"
                                                </A>
                                            }
                                        })}
                                </div>
                            },
                        )
                    })
            }}
            {move || {
                conflict()
                    .then(|| {
                        notice(
                            Tone::Warn,
                            view! {
                                <span>
                                    "Configuration changed elsewhere. Your edits are preserved; reload to review the latest version."
                                </span>
                                <div class="btn-group">{reload_button()}</div>
                            },
                        )
                    })
            }}
        </div>
    }
}
