//! `/settings` page: the daemon settings, each changed in its own field and
//! applied at once, then the host's settings, read-only.
use super::observability::Refresh;
use super::ui::{Icon, PageHeader, Tone, focus, icon, key_values, loading, notice, prose};
use super::{ClientErrorExt, UserError, access::can, confirm, live_row};
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{
    Client, ClientError,
    access::{GlobalPermission, Permission},
    observability::Bytes,
    system::{Pruning, Setting, SettingKind, SettingSource},
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

/// How often the page reads the settings again, for changes made elsewhere.
const REFRESH: Duration = Duration::from_secs(30);

/// `/settings` page. Each setting keeps its row across reloads, so a reload
/// never takes the focus or what is being typed, and one that fails shows
/// nothing over settings already on the page: the sidebar says when the
/// daemon can't be reached, and each field why its change failed.
#[component]
pub(super) fn SettingsPage() -> impl IntoView {
    let refresh = Refresh::every(REFRESH);
    let data = LocalResource::new(move || {
        refresh.track();
        async move { Client::browser().system_configuration().await.user_error() }
    });
    let manage = can(Permission::Global(GlobalPermission::SettingsManage));
    let settings = RwSignal::new(Vec::<Setting>::new());
    let groups = RwSignal::new(BTreeMap::new());
    Effect::new(move |_| {
        if let Some(Ok(configuration)) = data.get() {
            settings.set(configuration.settings);
            if groups.with_untracked(|groups| *groups != configuration.groups) {
                groups.set(configuration.groups);
            }
        }
    });
    let status = move || match data.get() {
        None => Some(loading("Loading settings…")),
        Some(Err(error)) if settings.with(Vec::is_empty) => Some(notice(Tone::Bad, error)),
        Some(_) => None,
    };
    view! {
        <PageHeader
            title="Settings"
            description="Daemon settings apply as soon as they are saved."
        />
        {status}
        <div class="stack">
            <For
                each={move || sections(settings.get())}
                key={|(title, settings)| {
                    let keys: Vec<String> = settings
                        .iter()
                        .map(|setting| setting.key.clone())
                        .collect();
                    (title.clone(), keys)
                }}
                children={move |(title, initial)| {
                    view! {
                        <section class="card">
                            <header>
                                <h3>{title}</h3>
                            </header>
                            <ul class="setting-list">
                                {initial
                                    .into_iter()
                                    .map(|initial| {
                                        Field::new(settings, refresh, manage, initial).row()
                                    })
                                    .collect_view()}
                            </ul>
                        </section>
                    }
                }}
            />
        </div>
        <Show when={move || groups.with(|groups| !groups.is_empty())}>
            <div class="section-header">
                <div>
                    <h2>"Host settings"</h2>
                    <p>"Set in the daemon's configuration file; a change needs a restart."</p>
                </div>
            </div>
            <div class="stack">
                {move || {
                    groups
                        .get()
                        .into_iter()
                        .map(|(group, values)| {
                            view! {
                                <section class="card">
                                    <header>
                                        <h3>{group}</h3>
                                    </header>
                                    {key_values(values)}
                                </section>
                            }
                        })
                        .collect_view()
                }}
            </div>
        </Show>
    }
}

/// The daemon settings by section, in the order the daemon lists them.
fn sections(settings: Vec<Setting>) -> Vec<(String, Vec<Setting>)> {
    let mut sections: Vec<(String, Vec<Setting>)> = Vec::new();
    for setting in settings {
        match sections.last_mut() {
            Some((section, settings)) if *section == setting.section => settings.push(setting),
            _ => sections.push((setting.section.clone(), vec![setting])),
        }
    }
    sections
}

/// A setting's value as people read it: `No limit` for `null`, `On` or
/// `Off` for a switch, `0 (forever)` for one that keeps history, and sizes
/// in bytes with their binary-prefixed size, such as `4294967296 (4.0 GiB)`.
fn display(setting: &Setting, value: &Value) -> String {
    match value {
        Value::Null => "No limit".into(),
        Value::Number(number)
            if setting.prunes == Some(Pruning::History) && number.as_u64() == Some(0) =>
        {
            "0 (forever)".into()
        }
        Value::Bool(true) => "On".into(),
        Value::Bool(false) => "Off".into(),
        Value::String(choice) => choice.clone(),
        Value::Number(number) => match number.as_u64() {
            Some(bytes) if setting.key.ends_with("_bytes") => {
                format!("{bytes} ({})", Bytes(bytes))
            }
            _ => number.to_string(),
        },
        value => value.to_string(),
    }
}

/// A value as its field holds it: empty for no limit, and `true` or `false`
/// for a switch.
fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(choice) => choice.clone(),
        value => value.to_string(),
    }
}

/// One setting's row: its field, and the change it is saving.
#[derive(Clone, Copy)]
struct Field {
    /// The setting as the daemon last reported it.
    setting: Memo<Setting>,
    /// Every setting; a saved change replaces this one there.
    settings: RwSignal<Vec<Setting>>,
    /// Reads the settings again after a change, in case a read already
    /// running brings back the value from before it.
    refresh: Refresh,
    /// Whether the account may change settings.
    manage: Signal<bool>,
    /// Whether the field takes changes: the account may make them, and the
    /// installation has what the setting needs.
    editable: Memo<bool>,
    /// What the field shows: the setting's value, until typed over.
    draft: RwSignal<String>,
    /// Whether a number typed over the setting's value isn't saved, nor
    /// being saved.
    unsaved: Memo<bool>,
    /// Whether a change is being sent.
    saving: RwSignal<bool>,
    /// What the last change did, such as `Saved` or `Reset, was 30`, and
    /// the value it left: the note says so while the setting keeps it.
    done: RwSignal<Option<(String, Value)>>,
    /// Why the last change was refused.
    refused: RwSignal<Option<String>>,
}

impl Field {
    /// The field of `initial`, following its setting in `settings`.
    fn new(
        settings: RwSignal<Vec<Setting>>,
        refresh: Refresh,
        manage: Signal<bool>,
        initial: Setting,
    ) -> Self {
        let draft = RwSignal::new(text(&initial.value));
        let live = live_row(settings, initial, |setting| &setting.key);
        let setting = Memo::new(move |_| live.get());
        let editable =
            Memo::new(move |_| manage.get() && setting.with(|setting| setting.needs.is_none()));
        let saving = RwSignal::new(false);
        let unsaved = Memo::new(move |_| {
            !saving.get()
                && setting.with(|setting| {
                    matches!(setting.kind, SettingKind::Integer { .. })
                        && draft.with(|draft| draft.trim() != text(&setting.value))
                })
        });
        let done = RwSignal::new(None::<(String, Value)>);
        Effect::new(move |shown: Option<String>| {
            setting.with(|setting| {
                let value = text(&setting.value);
                if shown.is_none_or(|shown| draft.with_untracked(|draft| draft.trim() == shown)) {
                    draft.set(value.clone());
                }
                if done.with_untracked(|done| {
                    done.as_ref()
                        .is_some_and(|(_, left)| *left != setting.value)
                }) {
                    done.set(None);
                }
                value
            })
        });
        Self {
            setting,
            settings,
            refresh,
            manage,
            editable,
            draft,
            unsaved,
            saving,
            done,
            refused: RwSignal::new(None),
        }
    }

    /// The ID of the row's `part`: its field, or the description and the
    /// note that describe the field.
    fn id(self, part: &str) -> String {
        self.setting
            .with_untracked(|setting| format!("setting-{}-{part}", setting.key))
    }

    /// Moves the focus to the field, when the row has one, before the
    /// button that `click` pressed with the keyboard goes away. A tap or a
    /// mouse click leaves the focus alone: on a phone, focusing the field
    /// would bring the keypad back over the outcome.
    fn focus(self, click: &web_sys::MouseEvent) {
        if click.detail() == 0
            && let Some(field) = document().get_element_by_id(&self.id("field"))
        {
            focus(&field);
        }
    }

    /// Shows the setting's value again, dropping what was typed and why it
    /// was refused, unless it is being saved, which nothing takes back.
    fn revert(self) {
        if self.saving.get_untracked() {
            return;
        }
        self.draft
            .set(self.setting.with_untracked(|setting| text(&setting.value)));
        self.refused.set(None);
    }

    /// Sends `request`, which changes the setting, unless one is running.
    /// On success the note says `done`; a refusal shows there instead, and
    /// the field keeps what was typed, but not a switch or a choice.
    fn send(
        self,
        done: String,
        request: impl Future<Output = Result<Setting, ClientError>> + 'static,
    ) {
        if self.saving.get_untracked() {
            return;
        }
        self.saving.set(true);
        self.done.set(None);
        self.refused.set(None);
        spawn_local(async move {
            let outcome = request.await;
            self.saving.try_set(false);
            match outcome {
                Ok(saved) => {
                    self.done.try_set(Some((done, saved.value.clone())));
                    self.settings.try_update(|settings| {
                        if let Some(setting) =
                            settings.iter_mut().find(|setting| setting.key == saved.key)
                        {
                            *setting = saved;
                        }
                    });
                    self.refresh.request();
                }
                Err(error) => {
                    self.refused.try_set(Some(error.user_message()));
                    if let Some(setting) = self.setting.try_get_untracked()
                        && !matches!(setting.kind, SettingKind::Integer { .. })
                    {
                        self.draft.try_set(text(&setting.value));
                    }
                }
            }
        });
    }

    /// The value the field shows, or why it isn't one the setting takes,
    /// in fewer words than the daemon's refusal, which names the setting,
    /// and without a maximum nobody means to reach: the note has one line.
    fn typed(self) -> Result<Value, String> {
        let draft = self.draft.get_untracked();
        Ok(
            match self.setting.with_untracked(|setting| setting.kind.clone()) {
                SettingKind::Boolean => Value::Bool(draft == "true"),
                SettingKind::Choice { .. } => Value::String(draft),
                SettingKind::Integer { nullable: true, .. } if draft.trim().is_empty() => {
                    Value::Null
                }
                SettingKind::Integer {
                    minimum, maximum, ..
                } => match draft.trim().parse::<u64>() {
                    Ok(number)
                        if number >= minimum && maximum.is_none_or(|maximum| number <= maximum) =>
                    {
                        Value::from(number)
                    }
                    _ => {
                        let takes = SettingKind::Integer {
                            minimum,
                            maximum: maximum.filter(|&maximum| maximum < u64::from(u32::MAX)),
                            nullable: false,
                        };
                        return Err(format!("Enter {takes}."));
                    }
                },
            },
        )
    }

    /// Saves what the field shows, when it differs from the setting's
    /// value.
    fn save(self) {
        if self.saving.get_untracked() {
            return;
        }
        let value = match self.typed() {
            Ok(value) => value,
            Err(refused) => {
                self.refused.set(Some(refused));
                return;
            }
        };
        let (key, same) = self
            .setting
            .with_untracked(|setting| (setting.key.clone(), setting.value == value));
        if same {
            self.revert();
            return;
        }
        if !self.confirmed(&value) {
            return;
        }
        self.send("Saved".into(), async move {
            Client::browser().put_setting(&key, value).await
        });
    }

    /// Whether the change to `value` may go ahead: asked first when it
    /// keeps less than now, which deletes the rest for good. Declined, a
    /// number stays in its field, not saved.
    fn confirmed(self, value: &Value) -> bool {
        self.setting.with_untracked(|setting| {
            let deletes = match setting.prunes {
                _ if !setting.keeps_less(value) => return true,
                Some(Pruning::Images) => {
                    "That keeps fewer images, and fewer deployments in history: the next cleanup deletes the rest, from a build registry for good."
                }
                _ => "That keeps less history: piqueld deletes the rest for good.",
            };
            confirm(&format!(
                "Change {} from {} to {}? {deletes}",
                setting.label,
                display(setting, &setting.value),
                display(setting, value),
            ))
        })
    }

    /// Resets the setting to its default, once confirmed if it keeps less
    /// history; the note then says what it was. Pressed with the keyboard,
    /// `click` gives the field the focus, as the button goes away.
    fn reset(self, click: &web_sys::MouseEvent) {
        if !self.confirmed(
            &self
                .setting
                .with_untracked(|setting| setting.default.clone()),
        ) {
            return;
        }
        self.focus(click);
        let (key, was) = self
            .setting
            .with_untracked(|setting| (setting.key.clone(), display(setting, &setting.value)));
        self.send(format!("Reset, was {was}"), async move {
            Client::browser().reset_setting(&key).await
        });
    }

    /// The setting's row: what it is, then its field with the button
    /// resetting it and the note under them. The button's place and the
    /// note keep their size, so a change moves nothing; only a refusal from
    /// the daemon too long for the note takes a second line.
    fn row(self) -> impl IntoView {
        let setting = self.setting.get_untracked();
        view! {
            <li>
                <div class="setting-name">
                    <label for={self.id("field")}>{setting.label}</label>
                    <code>{setting.key}</code>
                    <div id={self.id("about")}>
                        {setting
                            .description
                            .lines()
                            .map(|paragraph| view! { <p class="hint">{prose(paragraph)}</p> })
                            .collect_view()}
                    </div>
                </div>
                <div class="setting-value">
                    <div class="setting-control">
                        <Show
                            when={move || self.editable.get()}
                            fallback={move || {
                                view! {
                                    <span class="setting-fixed">
                                        {move || {
                                            self.setting
                                                .with(|setting| match &setting.value {
                                                    Value::String(choice) => {
                                                        view! { <code>{choice.clone()}</code> }
                                                            .into_any()
                                                    }
                                                    value => display(setting, value).into_any(),
                                                })
                                        }}
                                    </span>
                                }
                            }}
                        >
                            {self.control()}
                        </Show>
                        {self.reset_button()}
                    </div>
                    <p class="setting-note" id={self.id("note")}>
                        {self.status()}
                        {self.note()}
                    </p>
                </div>
            </li>
        }
    }

    /// The icon button resetting a changed setting, with its default as its
    /// name. It keeps its place, hidden, while there is nothing to reset,
    /// the account may not, or a number isn't saved, so that Tab goes from
    /// the field to **Save**.
    fn reset_button(self) -> impl IntoView {
        let name = move || {
            self.setting.with(|setting| {
                format!(
                    "Reset {} to its default, {}",
                    setting.label,
                    display(setting, &setting.default)
                )
            })
        };
        view! {
            <button
                type="button"
                class="btn btn-ghost btn-icon"
                aria-label={name}
                title={name}
                disabled={move || {
                    !self.manage.get()
                        || self.unsaved.get()
                        || self.setting.with(|setting| setting.source != SettingSource::Changed)
                }}
                on:click={move |click| self.reset(&click)}
            >
                {icon(Icon::Restore)}
            </button>
        }
    }

    /// What a change is doing or did, announced to assistive technology:
    /// why it was refused, that it is being saved, or what it did.
    fn status(self) -> impl IntoView {
        let status = move || {
            self.refused
                .get()
                .map(|refused| ("bad", refused))
                .or_else(|| {
                    if self.saving.get() {
                        Some(("pending", "Saving…".to_owned()))
                    } else {
                        self.done.get().map(|(done, _)| ("ok", done))
                    }
                })
        };
        view! {
            <span
                class="setting-status"
                aria-live="polite"
                data-tone={move || status().map(|(tone, _)| tone)}
            >
                {move || status().map(|(_, message)| message)}
            </span>
        }
    }

    /// The rest of the note, for a number that isn't saved: that it isn't,
    /// unless its refusal says so, then **Save**, unless the page refused
    /// a number that saving would refuse again, and **Cancel**.
    /// Otherwise why the setting has no field, the default of a changed
    /// setting, or how to set no limit.
    fn note(self) -> impl Fn() -> AnyView {
        let button = move |name: &'static str, act: fn(Self)| {
            view! {
                <button
                    type="button"
                    on:click={move |click| {
                        self.focus(&click);
                        act(self);
                    }}
                >
                    {name}
                </button>
            }
        };
        move || {
            let refused = self.refused.with(Option::is_some);
            if self.unsaved.get() {
                return view! {
                    {(!refused).then(|| view! { <span class="setting-unsaved">"Not saved"</span> })}
                    {(!refused || self.typed().is_ok()).then(|| button("Save", Self::save))}
                    {button("Cancel", Self::revert)}
                }
                .into_any();
            }
            if refused {
                return ().into_any();
            }
            self.setting
                .with(|setting| {
                    if let Some(needs) = &setting.needs {
                        Some(format!("Applies only with {needs}"))
                    } else if setting.source == SettingSource::Changed {
                        Some(format!("Default: {}", display(setting, &setting.default)))
                    } else if self.editable.get()
                        && matches!(setting.kind, SettingKind::Integer { nullable: true, .. })
                    {
                        Some("Empty for no limit".to_owned())
                    } else {
                        None
                    }
                })
                .into_any()
        }
    }

    /// The field itself, by what the setting takes: a switch, a choice or
    /// a number.
    fn control(self) -> AnyView {
        let (key, kind) = self
            .setting
            .with_untracked(|setting| (setting.key.clone(), setting.kind.clone()));
        match kind {
            SettingKind::Boolean => self.switch().into_any(),
            SettingKind::Choice { values } => self.choice(values).into_any(),
            SettingKind::Integer { nullable, .. } => {
                self.number(nullable, key.ends_with("_bytes")).into_any()
            }
        }
    }

    /// What describes the field to assistive technology: its note, then
    /// the setting's description.
    fn described(self) -> String {
        format!("{} {}", self.id("note"), self.id("about"))
    }

    /// A switch, with `On` or `Off` beside it. It saves when changed, and
    /// takes no other change until that is saved.
    fn switch(self) -> impl IntoView {
        view! {
            <span class="setting-aside">
                {move || if self.draft.with(|draft| draft == "true") { "On" } else { "Off" }}
            </span>
            <input
                type="checkbox"
                role="switch"
                class="switch"
                id={self.id("field")}
                aria-describedby={self.described()}
                prop:checked={move || self.draft.with(|draft| draft == "true")}
                on:click={move |click| {
                    if self.saving.get_untracked() {
                        click.prevent_default();
                    }
                }}
                on:change={move |event| {
                    self.draft.set(event_target_checked(&event).to_string());
                    self.save();
                }}
            />
        }
    }

    /// A choice among `values`. It saves when changed, and takes no other
    /// change until that is saved.
    fn choice(self, values: Vec<String>) -> impl IntoView {
        view! {
            <span class="field">
                <select
                    id={self.id("field")}
                    aria-describedby={self.described()}
                    on:change={move |event| {
                        if self.saving.get_untracked() {
                            event_target::<web_sys::HtmlSelectElement>(&event)
                                .set_value(&self.draft.get_untracked());
                            return;
                        }
                        self.draft.set(event_target_value(&event));
                        self.save();
                    }}
                >
                    {values
                        .into_iter()
                        .map(|choice| {
                            let (name, chosen) = (choice.clone(), choice.clone());
                            view! {
                                <option
                                    value={choice}
                                    prop:selected={move || self.draft.with(|draft| *draft == chosen)}
                                >
                                    {name}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </span>
        }
    }

    /// A number, empty for no limit when `nullable`. It saves on Enter, or
    /// with **Save** in the note, and Escape drops it: leaving the field
    /// does neither, so a half-typed number is never saved. A size in
    /// `bytes` shows its binary-prefixed size beside it.
    fn number(self, nullable: bool, bytes: bool) -> impl IntoView {
        view! {
            {bytes
                .then(|| {
                    view! {
                        <span class="setting-aside">
                            {move || {
                                self.draft
                                    .with(|draft| draft.trim().parse().ok())
                                    .map(|bytes| Bytes(bytes).to_string())
                            }}
                        </span>
                    }
                })}
            <span class="field">
                <input
                    type="text"
                    inputmode="numeric"
                    autocomplete="off"
                    id={self.id("field")}
                    aria-describedby={self.described()}
                    aria-invalid={move || self.refused.with(Option::is_some).then_some("true")}
                    data-unsaved={move || self.unsaved.get()}
                    placeholder={nullable.then_some("No limit")}
                    readonly={move || self.saving.get()}
                    prop:value={move || self.draft.get()}
                    on:input={move |event| {
                        self.draft.set(event_target_value(&event));
                        self.refused.set(None);
                        self.done.set(None);
                    }}
                    on:keydown={move |event| match event.key().as_str() {
                        "Enter" => self.save(),
                        "Escape" => self.revert(),
                        _ => {}
                    }}
                />
            </span>
        }
    }
}
