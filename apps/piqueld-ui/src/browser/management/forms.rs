//! Form state shared by the editor's settings: a group's unsaved draft, the
//! dialog editing one row of a list, and a list's rows edited in place.
use super::editor;
use crate::browser::ui::{Icon, icon, limited_add_button, row_actions, text_field};
use leptos::prelude::*;
use piqueld_client::{ApplicationManifest, access::AppPermission, edit::ApplicationEdit};

/// One settings group's form: its `value` and the saved `baseline` it
/// compares against. While they differ the group is in the editor's dirty
/// set, which blocks deploys and guards navigation.
pub(super) struct Draft<T: Send + Sync + 'static> {
    /// The edited value.
    pub(super) value: RwSignal<T>,
    /// The saved value the edits are compared against.
    pub(super) baseline: RwSignal<T>,
}

impl<T: Send + Sync + 'static> Clone for Draft<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Send + Sync + 'static> Copy for Draft<T> {}

impl<T: Clone + PartialEq + Send + Sync + 'static> Draft<T> {
    /// Starts editing `initial` as the dirty group `key`, which it leaves
    /// when the owning view unmounts.
    pub(super) fn new(key: String, initial: T) -> Self {
        let draft = Self {
            value: RwSignal::new(initial.clone()),
            baseline: RwSignal::new(initial),
        };
        editor().guard(key, move || draft.changed());
        draft
    }

    /// Starts editing what `read` reads from the saved manifest. The
    /// baseline follows later saves, fetches, and reloads, and so does the
    /// form until it is edited: edits are kept, and discarding them returns
    /// to the latest saved value.
    pub(super) fn synced(
        key: impl Into<String>,
        read: impl Fn(&ApplicationManifest) -> T + 'static,
    ) -> Self {
        let context = editor();
        let draft = Self::new(key.into(), read(&context.manifest()));
        Effect::new(move |_| draft.follow(read(&context.manifest())));
        draft
    }

    /// Moves the baseline to `saved`, and the form too unless it is edited.
    pub(super) fn follow(self, saved: T) {
        if self.value.get_untracked() == self.baseline.get_untracked() {
            self.reset(saved);
        } else {
            self.baseline.set(saved);
        }
    }

    /// Whether the form differs from its baseline.
    pub(super) fn changed(self) -> bool {
        self.value
            .with(|value| self.baseline.with(|baseline| value != baseline))
    }

    /// Restarts the form at `value`, as saved.
    pub(super) fn reset(self, value: T) {
        self.value.set(value.clone());
        self.baseline.set(value);
    }

    /// Drops the edits.
    pub(super) fn discard(self) {
        self.value.set(self.baseline.get_untracked());
    }

    /// Saves `edit` as the group's change, then restarts the form at what
    /// `read` reads from the saved manifest.
    pub(super) fn save(self, edit: ApplicationEdit, read: fn(&ApplicationManifest) -> T) {
        editor().save(edit, move |saved| {
            self.reset(read(&saved.application.to_manifest()));
        });
    }

    /// Save and discard buttons, shown only while the form has edits.
    pub(super) fn actions(
        self,
        save: impl Fn() + Copy + 'static,
        disabled: impl Fn() -> bool + Copy + Send + Sync + 'static,
    ) -> impl IntoView {
        view! {
            <div class="form-actions" hidden={move || !self.changed()}>
                <button
                    type="button"
                    class="btn btn-primary"
                    disabled={move || !self.changed() || disabled()}
                    on:click={move |_| save()}
                >
                    "Save changes"
                </button>
                <button type="button" class="btn btn-ghost" on:click={move |_| self.discard()}>
                    "Discard"
                </button>
                <span class="status">"Unsaved edits"</span>
            </div>
        }
    }
}

impl Draft<String> {
    /// A page title showing the saved name, with an inline rename form
    /// behind the pencil button labelled `rename`. The form is open while
    /// `editing` holds; submitting it runs `save`, and `disabled` blocks
    /// both opening and submitting it. The pencil is hidden without
    /// `apps:write`.
    pub(super) fn rename_title(
        self,
        rename: &'static str,
        label: &'static str,
        editing: RwSignal<bool>,
        disabled: impl Fn() -> bool + Copy + Send + Sync + 'static,
        save: impl Fn() + 'static,
    ) -> impl IntoView {
        let context = editor();
        view! {
            <h1 hidden={move || editing.get()}>{move || self.baseline.get()}</h1>
            <button
                type="button"
                class="btn btn-ghost btn-icon"
                aria-label={rename}
                title={rename}
                hidden={move || editing.get() || !context.can(AppPermission::Write)}
                disabled={disabled}
                on:click={move |_| editing.set(true)}
            >
                {icon(Icon::Pencil)}
            </button>
            <form
                class="form-row"
                hidden={move || !editing.get()}
                on:submit={move |event| {
                    event.prevent_default();
                    save();
                }}
            >
                <fieldset class="form-row" disabled={disabled}>
                    {text_field(label, self.value)}
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || !self.changed()}
                    >
                        "Save"
                    </button>
                    <button
                        type="button"
                        class="btn btn-ghost"
                        on:click={move |_| {
                            self.discard();
                            editing.set(false);
                        }}
                    >
                        "Cancel"
                    </button>
                </fieldset>
            </form>
        }
    }
}

/// The dialog of a list edited row by row: it adds a row, or edits the one
/// at `index`, as `draft`. Submitting saves the whole list at once, so the
/// list needs no unsaved state of its own. While open, the dialog is in the
/// editor's dirty set, so the saved rows its index points into are not
/// replaced under it. Closing it after a refused save keeps the editor's
/// error, with its Reload button, on the page.
pub(super) struct RowDialog<T: Send + Sync + 'static> {
    /// Whether the dialog is open.
    pub(super) opened: RwSignal<bool>,
    /// The row being edited.
    pub(super) draft: RwSignal<T>,
    /// Position of the row being edited; `None` when adding one.
    index: RwSignal<Option<usize>>,
}

impl<T: Send + Sync + 'static> Clone for RowDialog<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Send + Sync + 'static> Copy for RowDialog<T> {}

impl<T: Clone + Default + Send + Sync + 'static> RowDialog<T> {
    /// The dialog of the list `key`, which names it in the dirty set.
    pub(super) fn new(key: &'static str) -> Self {
        let opened = RwSignal::new(false);
        editor().guard(format!("{key}-dialog"), move || opened.get());
        Self {
            opened,
            draft: RwSignal::new(T::default()),
            index: RwSignal::new(None),
        }
    }

    /// The card's button labelled `label`, opening the dialog on a blank
    /// row. It is disabled while saves are blocked or the list is `full`.
    pub(super) fn add_button(
        self,
        label: &'static str,
        full: impl Fn() -> bool + Send + Sync + 'static,
    ) -> AnyView {
        let context = editor();
        limited_add_button(
            label,
            move || context.blocked() || full(),
            move || self.add(),
        )
    }

    /// Opens the dialog on a blank row.
    pub(super) fn add(self) {
        self.open(None, T::default());
    }

    /// Opens the dialog on `row`, the one at `index`.
    pub(super) fn edit(self, index: usize, row: T) {
        self.open(Some(index), row);
    }

    /// Opens the dialog on `row`, at `index` when editing and `None` when adding.
    fn open(self, index: Option<usize>, row: T) {
        editor().set_error(None);
        self.draft.set(row);
        self.index.set(index);
        self.opened.set(true);
    }

    /// `rows` with the draft in place of the edited row, or appended.
    pub(super) fn apply(self, mut rows: Vec<T>) -> Vec<T> {
        let draft = self.draft.get_untracked();
        match self.index.get_untracked() {
            Some(index) if index < rows.len() => rows[index] = draft,
            _ => rows.push(draft),
        }
        rows
    }

    /// The buttons of the row at `index`: **Edit**, opening the dialog on
    /// `row`, and **Remove**, running `remove`. Both are off while saves are
    /// blocked, when a save in flight may be about to move the rows.
    pub(super) fn row_actions(self, index: usize, row: T, remove: impl Fn() + 'static) -> AnyView {
        let context = editor();
        row_actions(
            move || context.blocked(),
            move || self.edit(index, row.clone()),
            remove,
        )
    }

    /// `rows` without the one at `index`, for the list's Remove buttons.
    pub(super) fn without(mut rows: Vec<T>, index: usize) -> Vec<T> {
        if index < rows.len() {
            rows.remove(index);
        }
        rows
    }

    /// Saves `edit`, the whole list, and closes the dialog once saved. A
    /// refused save leaves it open, showing the editor's error.
    pub(super) fn save(self, edit: ApplicationEdit) {
        editor().save(edit, move |_| self.opened.set(false));
    }

    /// `add` while the dialog adds a row, `edit` while it edits one.
    pub(super) fn title(self, add: &'static str, edit: &'static str) -> Signal<&'static str> {
        let index = self.index;
        Signal::derive(move || if index.get().is_some() { edit } else { add })
    }
}

/// Rows of a list form, edited in place by index. A row removed meanwhile
/// reads as empty and ignores edits.
pub(super) trait Rows<T> {
    /// Reads one field of row `index`.
    fn field<R: Default>(self, index: usize, read: impl Fn(&T) -> R) -> R;
    /// Updates row `index`.
    fn edit(self, index: usize, write: impl FnOnce(&mut T));
    /// Removes row `index`.
    fn remove(self, index: usize);
    /// Indices of every row, to key a `For` on.
    fn indices(self) -> Vec<usize>;
}

impl<T: Send + Sync + 'static> Rows<T> for RwSignal<Vec<T>> {
    fn field<R: Default>(self, index: usize, read: impl Fn(&T) -> R) -> R {
        self.with(|rows| rows.get(index).map_or_default(read))
    }

    fn edit(self, index: usize, write: impl FnOnce(&mut T)) {
        self.update(|rows| {
            if let Some(row) = rows.get_mut(index) {
                write(row);
            }
        });
    }

    fn remove(self, index: usize) {
        self.update(|rows| {
            if index < rows.len() {
                rows.remove(index);
            }
        });
    }

    fn indices(self) -> Vec<usize> {
        (0..self.with(Vec::len)).collect()
    }
}
