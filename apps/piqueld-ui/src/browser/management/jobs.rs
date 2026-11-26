//! One-shot jobs, which run in list order before each rollout. Each is a
//! row, added and edited in a dialog; saving replaces the whole list.
use super::super::confirm;
use super::super::ui::{FormModal, add_button, empty, remove_button, row_actions};
use super::{RowDialog, editor};
use crate::editor::ServiceForm;
use leptos::prelude::*;
use piqueld_client::{
    ApplicationManifest, Job, JobRun, JobVolumeAccess, SecretMount, edit::ApplicationEdit,
};

/// One editable job. The command keeps one element per row, without shell parsing.
#[derive(Clone)]
struct JobDraft {
    name: String,
    service: String,
    command: Vec<String>,
    /// Environment variables the job adds, as key/value rows.
    environment: Vec<(String, String)>,
    /// Secrets the job adds, as name/container path rows.
    secrets: Vec<(String, String)>,
    timeout: String,
    volume_access: JobVolumeAccess,
    stop_service: bool,
}

impl Default for JobDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            service: String::new(),
            command: vec![String::new()],
            environment: Vec::new(),
            secrets: Vec::new(),
            timeout: Job::DEFAULT_TIMEOUT_SECONDS.to_string(),
            volume_access: JobVolumeAccess::default(),
            stop_service: false,
        }
    }
}

impl JobDraft {
    /// The manifest's jobs as editable rows.
    fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest.spec.jobs.iter().cloned().map(Self::from).collect()
    }

    /// What the job runs, as its row says it: its service, timeout and
    /// command. An empty command element, or one holding spaces, is quoted,
    /// since elements are never split.
    fn summary(&self) -> String {
        let command = self
            .command
            .iter()
            .map(|element| {
                if element.is_empty() || element.contains(char::is_whitespace) {
                    format!("'{element}'")
                } else {
                    element.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!("on {}, up to {}s: {command}", self.service, self.timeout)
    }

    /// What the job adds to its service's container, as its row says it:
    /// its environment variables and secret files, or nothing.
    fn additions(&self) -> Option<String> {
        let environment = self
            .environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"));
        let secrets = self
            .secrets
            .iter()
            .map(|(name, target)| format!("secret {name} at {target}"));
        let additions = environment.chain(secrets).collect::<Vec<_>>();
        (!additions.is_empty()).then(|| format!("adds {}", additions.join(", ")))
    }

    /// The job input, or a message when an environment key repeats or the
    /// timeout does not parse.
    #[expect(
        clippy::map_err_ignore,
        reason = "the form states what to enter, which a parse error cannot improve on"
    )]
    fn job(self) -> Result<Job, String> {
        Ok(Job {
            name: self.name,
            service: self.service,
            command: self
                .command
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            environment: ServiceForm::unique_keys("Environment", &self.environment)?,
            secrets: self
                .secrets
                .into_iter()
                .map(|(name, target)| SecretMount {
                    name: name.into(),
                    target,
                })
                .collect(),
            run: JobRun::BeforeRollout,
            timeout_seconds: self
                .timeout
                .parse()
                .map_err(|_| "Job timeout must be a whole number of seconds".to_owned())?,
            volume_access: self.volume_access,
            stop_service: self.stop_service,
        })
    }
}

impl From<Job> for JobDraft {
    fn from(job: Job) -> Self {
        Self {
            name: job.name,
            service: job.service,
            command: job.command.iter().map(ToString::to_string).collect(),
            environment: job
                .environment
                .into_iter()
                .map(|(key, value)| (key, value.to_string()))
                .collect(),
            secrets: job
                .secrets
                .into_iter()
                .map(|secret| (secret.name.to_string(), secret.target))
                .collect(),
            timeout: job.timeout_seconds.to_string(),
            volume_access: job.volume_access,
            stop_service: job.stop_service,
        }
    }
}

/// Command elements of the `job` in the dialog, one row each, plus a button
/// adding one.
fn command_rows(job: RwSignal<JobDraft>) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..job.with(|j| j.command.len())).collect::<Vec<_>>()}
                key={|element| *element}
                children={move |element| {
                    view! {
                        <div class="form-row">
                            <label class="field">
                                <span>"Command element"</span>
                                <input
                                    type="text"
                                    prop:value={move || {
                                        job.with(|j| {
                                            j.command.get(element).cloned().unwrap_or_default()
                                        })
                                    }}
                                    on:input={move |event| {
                                        job.update(|j| {
                                            if let Some(value) = j.command.get_mut(element) {
                                                *value = event_target_value(&event);
                                            }
                                        });
                                    }}
                                />
                            </label>
                            {remove_button(move || {
                                job.update(|j| {
                                    if element < j.command.len() {
                                        j.command.remove(element);
                                    }
                                });
                            })}
                        </div>
                    }
                }}
            />
        </div>
        <div>
            {add_button(
                "Add command element",
                move || job.update(|j| j.command.push(String::new())),
            )}
        </div>
    }
    .into_any()
}

/// A two-column list of rows a job adds to its service's container.
#[derive(Clone, Copy)]
struct PairRows {
    title: &'static str,
    labels: [&'static str; 2],
    add: &'static str,
    /// Values of a newly added row.
    new: [&'static str; 2],
    read: fn(&JobDraft) -> &Vec<(String, String)>,
    write: fn(&mut JobDraft) -> &mut Vec<(String, String)>,
}

impl PairRows {
    const ENVIRONMENT: Self = Self {
        title: "Environment",
        labels: ["Key", "Value"],
        add: "Add variable",
        new: ["", ""],
        read: |j| &j.environment,
        write: |j| &mut j.environment,
    };
    const SECRETS: Self = Self {
        title: "Secrets",
        labels: ["Secret name", "Container path"],
        add: "Add secret file",
        new: ["", "/run/secrets/"],
        read: |j| &j.secrets,
        write: |j| &mut j.secrets,
    };

    /// The `column` of `row`, as an input labelled `label` editing it.
    fn input(
        self,
        job: RwSignal<JobDraft>,
        row: usize,
        label: &'static str,
        column: fn(&mut (String, String)) -> &mut String,
    ) -> AnyView {
        view! {
            <label class="field">
                <span>{label}</span>
                <input
                    type="text"
                    prop:value={move || {
                        job.with(|j| {
                            (self.read)(j)
                                .get(row)
                                .cloned()
                                .map_or_default(|mut pair| std::mem::take(column(&mut pair)))
                        })
                    }}
                    on:input={move |event| {
                        job.update(|j| {
                            if let Some(pair) = (self.write)(j).get_mut(row) {
                                *column(pair) = event_target_value(&event);
                            }
                        });
                    }}
                />
            </label>
        }
        .into_any()
    }

    /// The rows of the `job` in the dialog, one per pair, plus a button
    /// adding one.
    fn view(self, job: RwSignal<JobDraft>) -> AnyView {
        view! {
            <div>
                <h4 class="hint mb-sm">
                    {self.title}
                </h4>
                <div class="form-list">
                    <For
                        each={move || (0..job.with(|j| (self.read)(j).len())).collect::<Vec<_>>()}
                        key={|row| *row}
                        children={move |row| {
                            view! {
                                <div class="form-row">
                                    {self.input(job, row, self.labels[0], |pair| &mut pair.0)}
                                    {self.input(job, row, self.labels[1], |pair| &mut pair.1)}
                                    {remove_button(move || {
                                        job.update(|j| {
                                            let pairs = (self.write)(j);
                                            if row < pairs.len() {
                                                pairs.remove(row);
                                            }
                                        });
                                    })}
                                </div>
                            }
                        }}
                    />
                </div>
                {add_button(
                    self.add,
                    move || {
                        job.update(|j| (self.write)(j).push(self.new.map(String::from).into()));
                    },
                )}
            </div>
        }
        .into_any()
    }
}

/// Jobs of the saved configuration, in the order they run: each a row saying
/// what it runs and what it adds to its service's container, moved up or
/// down, and added and edited in a dialog. A row cuts a long line short, so
/// its title holds it whole. Every change validates the timeouts and saves
/// the whole list at once.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn JobSettings() -> impl IntoView {
    let context = editor();
    let dialog = RowDialog::<JobDraft>::new("jobs");
    let job = dialog.draft;
    let save = move |rows: Vec<JobDraft>| match rows.into_iter().map(JobDraft::job).collect() {
        Ok(jobs) => dialog.save(ApplicationEdit::Jobs(jobs)),
        Err(message) => context.set_error(Some(message)),
    };
    let remove = move |index: usize| {
        if !confirm("Remove this job from the saved configuration? Later deployments skip it.") {
            return;
        }
        save(dialog.without(JobDraft::rows(&context.manifest()), index));
    };
    let swap = move |first: usize| {
        let mut rows = JobDraft::rows(&context.manifest());
        if first + 1 < rows.len() {
            rows.swap(first, first + 1);
            save(rows);
        }
    };
    let full = move || context.manifest().spec.jobs.len() >= Job::MAX_PER_APPLICATION;
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Jobs"</h3>
                    <p>
                        "Each deployment runs these commands to completion, in order, after preparing images. A job reuses its service's image, environment, secrets, and volume mounts, and waits for that service's startup dependencies to be healthy. Environment variables and secrets added here reach only the job's container, never the service's replicas, and can't replace ones the service sets. Dependencies start or update first; other services roll out only after all jobs succeed. A failed job stops the deployment and dependencies return to the running deployment. Volumes are read-only unless the job has exclusive access, which stops every service using its writable volumes until the rollout. A job can also stop its own service, so its previous version never runs against what the job changes."
                    </p>
                </div>
                {dialog.add_button("Add job", full)}
            </header>
            {move || {
                let rows = JobDraft::rows(&context.manifest());
                if rows.is_empty() {
                    return empty("No jobs. Deployments roll out services directly.");
                }
                let last = rows.len() - 1;
                view! {
                    <ol class="list" aria-label="Jobs">
                        {rows
                            .into_iter()
                            .enumerate()
                            .map(|(index, row)| {
                                let edited = row.clone();
                                let line = |text: String| {
                                    let whole = text.clone();
                                    view! { <small title={whole}>{text}</small> }
                                };
                                view! {
                                    <li class="list-row">
                                        <span class="title">
                                            <code>{row.name.clone()}</code>
                                            {line(row.summary())}
                                            {row.additions().map(line)}
                                        </span>
                                        {(row.volume_access == JobVolumeAccess::Exclusive)
                                            .then(|| view! { <span class="tag">"exclusive volumes"</span> })}
                                        {row
                                            .stop_service
                                            .then(|| view! { <span class="tag">"stops its service"</span> })}
                                        <span class="btn-group">
                                            <button
                                                type="button"
                                                class="btn btn-sm btn-ghost"
                                                disabled={move || context.blocked() || index == 0}
                                                on:click={move |_| swap(index - 1)}
                                            >
                                                "Move up"
                                            </button>
                                            <button
                                                type="button"
                                                class="btn btn-sm btn-ghost"
                                                disabled={move || context.blocked() || index == last}
                                                on:click={move |_| swap(index)}
                                            >
                                                "Move down"
                                            </button>
                                        </span>
                                        {row_actions(
                                            move || dialog.edit(index, edited.clone()),
                                            move || remove(index),
                                        )}
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ol>
                }
                    .into_any()
            }}
            <FormModal
                title={dialog.title("Add job", "Edit job")}
                opened={dialog.opened}
                busy={context.busy}
                error={context.error}
                submit={dialog.title("Add job", "Save job")}
                on_submit={Callback::new(move |()| {
                    save(dialog.apply(JobDraft::rows(&context.manifest())));
                })}
                wide=true
            >
                <div class="form-grid">
                    <label class="field">
                        <span>"Name"</span>
                        <input
                            type="text"
                            placeholder="migrate"
                            prop:value={move || job.with(|j| j.name.clone())}
                            on:input={move |event| {
                                job.update(|j| j.name = event_target_value(&event));
                            }}
                        />
                    </label>
                    <label class="field">
                        <span>"Service"</span>
                        <select
                            prop:value={move || job.with(|j| j.service.clone())}
                            on:change={move |event| {
                                job.update(|j| j.service = event_target_value(&event));
                            }}
                        >
                            {context.service_options()}
                        </select>
                    </label>
                </div>
                <div class="form-grid">
                    <label class="field">
                        <span>"Timeout (seconds)"</span>
                        <input
                            type="number"
                            min="1"
                            max={Job::MAX_TIMEOUT_SECONDS}
                            prop:value={move || job.with(|j| j.timeout.clone())}
                            on:input={move |event| {
                                job.update(|j| j.timeout = event_target_value(&event));
                            }}
                        />
                    </label>
                    <label class="field">
                        <span>"Volume access"</span>
                        <select
                            prop:value={move || job.with(|j| j.volume_access.as_str())}
                            on:change={move |event| {
                                if let Ok(access) = event_target_value(&event).parse() {
                                    job.update(|j| j.volume_access = access);
                                }
                            }}
                        >
                            <option value={JobVolumeAccess::ReadOnly.as_str()}>"Read-only"</option>
                            <option value={JobVolumeAccess::Exclusive.as_str()}>"Exclusive"</option>
                        </select>
                    </label>
                </div>
                <label class="checkbox">
                    <input
                        type="checkbox"
                        prop:checked={move || job.with(|j| j.stop_service)}
                        on:change={move |event| {
                            job.update(|j| j.stop_service = event_target_checked(&event));
                        }}
                    />
                    <span>"Stop service first"</span>
                </label>
                <p class="hint">"Each command element is one row; no shell parsing is applied."</p>
                {command_rows(job)}
                {PairRows::ENVIRONMENT.view(job)}
                {PairRows::SECRETS.view(job)}
            </FormModal>
        </section>
    }
}
