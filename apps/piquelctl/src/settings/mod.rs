//! `settings`: the daemon's settings. Daemon settings, such as how long
//! history is kept, change while it runs, unless its configuration file pins
//! them; the host's settings only change in that file.
use crate::{
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::report},
};
use clap::Subcommand;
use piqueld_client::{Client, system::Setting};

#[derive(Debug, Subcommand)]
pub(crate) enum SettingsCommand {
    /// List the host's settings, read-only, and every daemon setting with its
    /// value and where it comes from: its default, set with `settings set`,
    /// or pinned by the daemon's configuration file.
    ///
    /// Needs `system:read`. It is what `settings` alone shows.
    List,
    /// Show one daemon setting: its value, default, range and what it does.
    ///
    /// Needs `system:read`.
    Get {
        /// The setting's key, such as `retention.event_days`, as `settings
        /// list` shows it.
        key: String,
    },
    /// Change a daemon setting. It applies at once, without a restart, and
    /// the change is recorded in the audit trail.
    ///
    /// A setting the daemon's configuration file sets is pinned: only the
    /// file changes it. One that needs what the installation lacks, such as
    /// `build.unavailable` without a build machine, can't be changed. Needs
    /// `settings:manage`.
    Set {
        /// The setting's key, such as `retention.event_days`.
        key: String,
        /// Its new value, as `settings get` says it takes: a whole number,
        /// `none` for no limit where the setting allows it, `true` or
        /// `false`, or one of its choices, such as `local`.
        value: String,
    },
    /// Reset a daemon setting to its default, at once.
    ///
    /// Needs `settings:manage`, and fails for a pinned setting like `set`.
    Reset {
        /// The setting's key, such as `retention.event_days`.
        key: String,
    },
}

impl SettingsCommand {
    /// Runs a settings command; `None`, `settings` alone, lists them.
    pub(crate) async fn run(
        command: Option<&Self>,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        match command.unwrap_or(&Self::List) {
            Self::List => console.emit(&client.system_configuration().await?),
            Self::Get { key } => {
                let configuration = client.system_configuration().await?;
                let setting = configuration
                    .settings
                    .into_iter()
                    .find(|setting| &setting.key == key)
                    .ok_or_else(|| {
                        CliError::new(
                            ErrorKind::Input,
                            format!(
                                "no daemon setting is named {key:?}; `piquelctl settings list` lists them"
                            ),
                        )
                    })?;
                console.emit(&setting)
            }
            Self::Set { key, value } => {
                console.emit(&client.put_setting(key, parse_value(value)).await?)
            }
            Self::Reset { key } => console.emit(&client.reset_setting(key).await?),
        }
    }
}

/// Reads a setting's value from the command line: `none` (or `null`) is no
/// limit, `true` and `false` themselves, a number itself, and anything else
/// a string, which the daemon checks against the setting.
///
/// ```text
/// "30"    -> 30
/// "none"  -> null
/// "false" -> false
/// "local" -> "local"
/// ```
fn parse_value(value: &str) -> serde_json::Value {
    match value {
        "none" | "null" => serde_json::Value::Null,
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        value => value
            .parse::<u64>()
            .map_or_else(|_| value.into(), serde_json::Value::from),
    }
}

/// A setting's value as people read it: `none` for no limit.
pub(crate) fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "none".into(),
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

report!(Setting, self, out, {
    out.label(&self.key, display_value(&self.value))?;
    out.label("  Name", &self.label)?;
    out.label("  Source", self.source.label())?;
    out.label("  Default", display_value(&self.default))?;
    out.label("  Takes", &self.kind)?;
    if let Some(needs) = &self.needs {
        out.label("  Applies only with", needs)?;
    }
    for paragraph in self.description.lines() {
        out.line(format_args!("  {paragraph}"))?;
    }
    Ok(())
});

#[cfg(test)]
mod tests;
