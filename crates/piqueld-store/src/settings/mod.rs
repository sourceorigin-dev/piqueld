//! Daemon settings: the values set through the API, kept in
//! `daemon_settings`, and the effective settings every part of the daemon
//! reads while it runs (see `piqueld_core::settings`).
use crate::{Author, Store, StoreError};
use piqueld_core::access::{GlobalPermission, Permission};
use piqueld_core::api::Setting;
use piqueld_core::settings::{Requirement, SettingLayers, SettingSpec, Settings};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The permission every setting change needs.
const MANAGE: Permission = Permission::Global(GlobalPermission::SettingsManage);

/// The settings' layers and the effective settings they resolve to.
#[derive(Debug)]
pub struct SettingsState {
    /// What the effective settings are made of.
    layers: SettingLayers,
    /// The settings the daemon applies.
    effective: Settings,
}

impl SettingsState {
    /// Resolves `layers`.
    fn new(layers: SettingLayers) -> Self {
        let effective = layers.effective();
        Self { layers, effective }
    }

    /// The settings the daemon applies.
    #[must_use]
    pub const fn effective(&self) -> &Settings {
        &self.effective
    }

    /// Changes `layers`, then resolves them again.
    fn change(&mut self, change: impl FnOnce(&mut SettingLayers)) {
        change(&mut self.layers);
        self.effective = self.layers.effective();
    }
}

impl Store {
    /// Reads the settings set through the API, by key.
    pub(crate) async fn stored_settings(
        pool: &sqlx::SqlitePool,
    ) -> Result<SettingsState, StoreError> {
        let rows = sqlx::query!("SELECT key AS \"key!\",value_json FROM daemon_settings")
            .fetch_all(pool)
            .await
            .map_err(StoreError::database)?;
        let stored = rows
            .into_iter()
            .map(|row| {
                let value = serde_json::from_str(&row.value_json).map_err(StoreError::corrupt)?;
                Ok((row.key, value))
            })
            .collect::<Result<BTreeMap<String, Value>, StoreError>>()?;
        for (key, value) in &stored {
            if let Err(error) = SettingSpec::find(key).and_then(|spec| spec.check(value)) {
                tracing::warn!(%key, %error, "ignoring a stored daemon setting this version doesn't take; it keeps its default");
            }
        }
        Ok(SettingsState::new(SettingLayers {
            stored,
            met: BTreeSet::new(),
        }))
    }

    /// Changes the settings `change` changes, in memory only, as though
    /// set through the API, so they apply at once. For tests, which skip
    /// the request, its permission check and its audit record.
    #[cfg(any(test, feature = "testing"))]
    pub fn assume_settings(&self, change: impl FnOnce(&mut Settings)) {
        self.settings.send_modify(|state| {
            state.change(|layers| {
                let before = layers.effective();
                let mut after = before.clone();
                change(&mut after);
                for spec in piqueld_core::settings::SETTINGS {
                    let value = spec.value_in(&after);
                    debug_assert!(spec.check(&value).is_ok(), "{}: {value}", spec.key);
                    if value != spec.value_in(&before) {
                        layers.stored.insert(spec.key.to_owned(), value);
                    }
                }
            });
        });
    }

    /// Records what the installation offers the settings that need it, such
    /// as a build machine: the API refuses to change the others. The daemon
    /// calls it once, at startup.
    pub fn meet_requirements(&self, met: BTreeSet<Requirement>) {
        self.settings
            .send_modify(|state| state.change(|layers| layers.met = met));
    }

    /// The settings in effect now. Read them where they apply, rather than
    /// once, so a change applies without a restart.
    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings.borrow().effective.clone()
    }

    /// Notifies after every settings change, for work scheduled by them,
    /// such as the reconciliation scan.
    #[must_use]
    pub fn settings_changes(&self) -> tokio::sync::watch::Receiver<SettingsState> {
        self.settings.subscribe()
    }

    /// Every daemon setting as the API reports it, in declaration order.
    #[must_use]
    pub fn setting_views(&self) -> Vec<Setting> {
        self.settings.borrow().layers.views()
    }

    /// Sets the setting `key` to `value`, or resets it to its default when
    /// `None`, which the author's actor needs `settings:manage` for, checked
    /// in the same transaction, which also records the change in the audit
    /// trail. It applies once committed, while the writer is still held, so
    /// concurrent changes apply in the order they commit.
    ///
    /// # Errors
    /// Refusal, an unknown setting, a value it doesn't take or one that
    /// doesn't apply here, or storage errors.
    pub async fn set_setting(
        &self,
        author: impl Into<Author<'_>>,
        key: &str,
        value: Option<Value>,
    ) -> Result<Setting, StoreError> {
        let spec = SettingSpec::find(key)?;
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        author.actor.require_on(&mut tx, MANAGE, None).await?;
        let before = {
            let state = self.settings.borrow();
            state.layers.check(spec, value.as_ref())?;
            state.layers.view(spec).value
        };
        let change = if let Some(value) = &value {
            let json = value.to_string();
            sqlx::query!(
                "INSERT INTO daemon_settings(key,value_json) VALUES(?1,?2)
                ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json",
                spec.key,
                json
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            format!("setting {} changed from {before} to {value}", spec.key)
        } else {
            sqlx::query!("DELETE FROM daemon_settings WHERE key=?1", spec.key)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            format!(
                "setting {} reset from {before} to its default {}",
                spec.key,
                spec.default_value()
            )
        };
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        self.settings.send_modify(|state| {
            state.change(|layers| match value {
                Some(value) => {
                    layers.stored.insert(spec.key.to_owned(), value);
                }
                None => {
                    layers.stored.remove(spec.key);
                }
            });
        });
        Ok(self.settings.borrow().layers.view(spec))
    }
}

#[cfg(test)]
mod tests;
