//! Daemon settings: the installation's policy, such as how long history is
//! kept, which the API, `piquelctl settings` and the dashboard change while
//! the daemon runs. The `settings!` macro below declares each setting once,
//! generating the section types the daemon reads ([`Settings`]) and
//! [`SETTINGS`], through which the API checks and applies values by key.
//! [`SettingLayers`] resolves them: the stored value, else the default. The
//! configuration file holds only host facts, in `piqueld-config`, and
//! refuses a daemon setting.

use crate::api::{Setting, SettingKind, SettingSource};
use crate::build_machine::WhenUnavailable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

#[cfg(test)]
mod tests;

/// The values a setting takes, which [`SettingSpec::check`] checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Values {
    /// A whole number in a range, or `null` where `nullable`, for no limit.
    Integer {
        /// The smallest.
        minimum: u64,
        /// The largest.
        maximum: u64,
        /// Whether `null` is a value.
        nullable: bool,
    },
    /// `true` or `false`.
    Boolean,
    /// One of these strings.
    Choice(&'static [&'static str]),
}

impl Values {
    /// These values within `minimum..=maximum`, for a declared range.
    const fn within(self, minimum: u64, maximum: Option<u64>) -> Self {
        match self {
            Self::Integer {
                maximum: whole,
                nullable,
                ..
            } => Self::Integer {
                minimum,
                maximum: match maximum {
                    Some(maximum) => maximum,
                    None => whole,
                },
                nullable,
            },
            other => other,
        }
    }

    /// Whether `value` is one of these.
    fn contain(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Integer { nullable, .. }, Value::Null) => nullable,
            (
                Self::Integer {
                    minimum, maximum, ..
                },
                Value::Number(number),
            ) => number
                .as_u64()
                .is_some_and(|number| (minimum..=maximum).contains(&number)),
            (Self::Boolean, Value::Bool(_)) => true,
            (Self::Choice(choices), Value::String(choice)) => choices.contains(&choice.as_str()),
            _ => false,
        }
    }

    /// These values as the API reports them. The maximum of an integer
    /// bounded only by `u64` is left out: no one means to reach it.
    fn kind(self) -> SettingKind {
        match self {
            Self::Integer {
                minimum,
                maximum,
                nullable,
            } => SettingKind::Integer {
                minimum,
                maximum: (maximum < u64::MAX).then_some(maximum),
                nullable,
            },
            Self::Boolean => SettingKind::Boolean,
            Self::Choice(choices) => SettingKind::Choice {
                values: choices.iter().map(|&choice| choice.to_owned()).collect(),
            },
        }
    }
}

/// A setting's value type: the values it takes, which a declared range
/// narrows, and their JSON form.
pub trait SettingType {
    /// Every value of the type.
    const VALUES: Values;
    /// The value as JSON.
    fn to_json(&self) -> Value;
}

/// Implements [`SettingType`] for unsigned integers, and their `Option`s,
/// whose `None` is written `null`.
macro_rules! setting_types {
    ($($type:ty),*) => {$(
        impl SettingType for $type {
            const VALUES: Values = Values::Integer {
                minimum: 0,
                maximum: <$type>::MAX as u64,
                nullable: false,
            };
            fn to_json(&self) -> Value {
                Value::from(*self)
            }
        }
        impl SettingType for Option<$type> {
            const VALUES: Values = Values::Integer {
                minimum: 0,
                maximum: <$type>::MAX as u64,
                nullable: true,
            };
            fn to_json(&self) -> Value {
                Value::from(*self)
            }
        }
    )*};
}
setting_types!(u16, u32, u64);

impl SettingType for bool {
    const VALUES: Values = Values::Boolean;
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
}

impl SettingType for WhenUnavailable {
    const VALUES: Values = Values::Choice(&["fail", "local"]);
    fn to_json(&self) -> Value {
        Value::from(match self {
            Self::Fail => "fail",
            Self::Local => "local",
        })
    }
}

/// What a setting needs of the installation to apply. Without it, the API
/// refuses to change the setting.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Requirement {
    /// A build machine, `build.builder`.
    BuildMachine,
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BuildMachine => "a build machine, build.builder",
        })
    }
}

/// A setting's values: its type's, within its declared range, if any.
macro_rules! values {
    ($type:ty) => {
        <$type as SettingType>::VALUES
    };
    ($type:ty, $min:tt) => {
        <$type as SettingType>::VALUES.within($min, None)
    };
    ($type:ty, $min:tt, $max:tt) => {
        <$type as SettingType>::VALUES.within($min, Some($max))
    };
}

/// A setting's requirement, if it declares one.
macro_rules! requirement {
    () => {
        None
    };
    ($requirement:ident) => {
        Some(Requirement::$requirement)
    };
}

/// Whether a setting declares that it keeps history, with `keeps history`.
macro_rules! keeps {
    () => {
        false
    };
    (history) => {
        true
    };
}

/// Declares every daemon setting, grouped in sections, whose keys are
/// `section.field`:
///
/// ```text
/// /// The section's description.
/// section("Title") SectionType {
///     /// The setting's description.
///     field("Label"): type = default, from min to max, needs Requirement, keeps history;
/// }
/// ```
///
/// An integer's range is optional, and may be open (`at least 1`). Its
/// bounds are token trees, which `utoipa`'s derive reads unlike `literal`s;
/// one beyond `i32` needs a suffix, such as `_u64`. `needs` names a
/// [`Requirement`]. `keeps history` marks a setting whose lower value
/// deletes what a higher one kept, where `0`, if it takes it, keeps
/// everything. The type implements [`SettingType`].
macro_rules! settings {
    ($(
        $(#[doc = $section_doc:literal])*
        $section:ident($title:literal) $Section:ident {
            $(
                $(#[doc = $doc:literal])*
                $field:ident($label:literal): $type:ty = $default:expr
                    $(, from $min:tt to $max:tt)? $(, at least $least:tt)?
                    $(, needs $requirement:ident)? $(, keeps $kept:ident)?;
            )*
        }
    )*) => {
        $(
            $(#[doc = $section_doc])*
            #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
            #[serde(default, deny_unknown_fields)]
            pub struct $Section {
                $(
                    $(#[doc = $doc])*
                    $(#[schema(minimum = $min, maximum = $max)])?
                    $(#[schema(minimum = $least)])?
                    pub $field: $type,
                )*
            }

            impl Default for $Section {
                fn default() -> Self {
                    Self { $($field: $default,)* }
                }
            }
        )*

        /// Every daemon setting's value.
        #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
        pub struct Settings {
            $(
                $(#[doc = $section_doc])*
                pub $section: $Section,
            )*
        }

        /// Every daemon setting, in declaration order.
        pub const SETTINGS: &[SettingSpec] = &[$($(
            SettingSpec {
                key: concat!(stringify!($section), ".", stringify!($field)),
                section: $title,
                label: $label,
                doc: concat!($($doc, "\n",)*),
                values: values!($type $(, $min, $max)? $(, $least)?),
                requires: requirement!($($requirement)?),
                prunes: keeps!($($kept)?),
                value: |settings| settings.$section.$field.to_json(),
                apply: |settings, value| {
                    settings.$section.$field = serde_json::from_value(value.clone())?;
                    Ok(())
                },
            },
        )*)*];
    };
}

settings! {
    /// How long history is kept. `0` keeps it forever.
    retention("History") RetentionConfig {
        /// Days finished operations are kept; `0` keeps them forever. Older ones
        /// are removed during reconciliation, except each environment's latest
        /// operation and those in its deployment history or with job cleanup
        /// pending.
        finished_operation_days("Finished operations (days)"): u64 = 10, keeps history;
        /// Days each environment's deployment history is kept; `0` keeps it
        /// forever. A deployment is removed once it is older than this and
        /// beyond the newest `deployments_per_environment`, never while its
        /// environment still needs it.
        deployment_days("Deployment history (days)"): u64 = 90, keeps history;
        /// Deployments each environment keeps however old they are; `0` keeps
        /// every deployment, release and build record, as for `deployment_days`.
        deployments_per_environment("Deployments per environment"): u32 = 50, keeps history;
        /// Days application events are kept; `0` keeps them forever. Deleting an
        /// application also removes its events.
        event_days("Application events (days)"): u64 = 90, keeps history;
        /// Days events not tied to an application, such as those about shared
        /// infrastructure, are kept; `0` keeps them forever.
        daemon_event_days("Daemon events (days)"): u64 = 90, keeps history;
        /// Days the audit trail of API requests is kept; `0` keeps it forever.
        audit_days("Audit trail (days)"): u64 = 365, keeps history;
        /// Days each environment's traffic counts are kept; `0` keeps them
        /// forever. Deleting an environment also removes its counts.
        traffic_days("Traffic counts (days)"): u64 = 90, keeps history;
    }

    /// How much output of each build is kept, and for how long. Builds
    /// themselves are listed until deployment history pruning removes their
    /// operation.
    build_history("Build output") BuildHistoryConfig {
        /// The most output kept per build, in bytes. Output within it is kept
        /// whole. Of longer output, a build keeps the first half and the end,
        /// with a note of how many bytes it left out between them when the note
        /// fits.
        log_max_bytes("Output kept per build (bytes)"): u32 = 4 * 1024 * 1024, from 1 to 67_108_864;
        /// Days a build's output is kept after it finishes. The build stays
        /// listed, marked as expired.
        log_retention_days("Output kept (days)"): u32 = 30, from 1 to 3650, keeps history;
    }

    /// Where images build.
    build("Builds") BuildSettings {
        /// What a deployment that has to build does while the build machine
        /// can't be reached: `fail`, the default, fails it with
        /// `builder_unavailable`, and `local` builds on this host's Docker Engine
        /// instead, where a build competes with the applications the host runs.
        unavailable("While the build machine is unreachable"): WhenUnavailable
            = WhenUnavailable::Fail, needs BuildMachine;
    }

    /// Cleanup of the images this installation builds. Cleanup keeps what every
    /// environment and preview runs or is deploying, and removes the rest. It
    /// never removes pulled images, images built by hand, or images other
    /// installations built on the same Docker Engine. With a build machine, it
    /// also deletes from the build registry the images this host's builds pushed
    /// that no deployment it keeps there uses.
    images("Images") ImagesConfig {
        /// How many of each environment's latest successful deployments also
        /// keep their images, so their releases can be deployed again.
        keep_deployments("Deployments kept per environment"): u32 = 3;
        /// How many of each environment's latest successful deployments keep
        /// their images in the build registry, `build.repository`: never fewer
        /// than `keep_deployments`. Without a build machine, it does nothing.
        registry_keep_deployments("Deployments kept in the registry"): u32 = 10;
        /// Seconds between cleanups. Cleanup also runs after each operation.
        cleanup_interval_seconds("Cleanup interval (seconds)"): u64 = 3600, from 1 to 86_400;
    }

    /// How many previews may exist, and what each preview service may use.
    ///
    /// Counts are checked when a preview is created, so lowering one below the
    /// current count keeps every existing preview running and only refuses new
    /// ones. Resource bounds apply when a preview's manifest is rendered, so a
    /// preview picks up changed bounds on its next deployment. Environments are
    /// never bounded.
    previews("Previews") PreviewLimits {
        /// Previews one application may have; creating more fails with
        /// `preview_limit_reached`. `0` refuses every new preview.
        max_per_application("Previews per application"): u32 = 10;
        /// Previews the whole installation may have; creating more fails with
        /// `preview_limit_reached`. `0` refuses every new preview.
        max_total("Previews in total"): u32 = 30;
        /// CPU limit, in millicores, of preview services that set none.
        default_cpu_millis("Default CPU (millicores)"): u32 = 500, from 1 to 1_048_576;
        /// Memory limit, in bytes, of preview services that set none.
        default_memory_bytes("Default memory (bytes)"): u64 = 512 * 1024 * 1024,
            from 1 to 9_223_372_036_854_775_807_u64;
        /// Most replicas a preview service runs; manifests asking for more are
        /// capped with a warning.
        max_replicas("Replicas per service"): u16 = 1, from 1 to 100;
    }

    /// The managed gateway that serves applications' routes.
    ingress("Ingress") IngressSettings {
        /// Count the requests each route serves, for each environment's
        /// traffic page, while `ingress.enabled` is set. piqueld stores hourly
        /// totals, not the requests themselves or visitors' addresses.
        traffic("Count traffic"): bool = true;
    }

    /// How often piqueld checks that Docker matches what is deployed, and how
    /// long a deployment may take.
    reconciliation("Reconciliation") ReconciliationConfig {
        /// Seconds between full scans for drift between Docker and what is
        /// deployed.
        scan_interval_seconds("Scan interval (seconds)"): u64 = 60, from 1 to 86_400;
        /// Seconds a deployment may take to prepare: fetching its manifest,
        /// resolving images and building them, including the time spent waiting
        /// for a build slot.
        prepare_timeout_seconds("Preparation timeout (seconds)"): u64 = 300, from 1 to 86_400;
        /// Seconds a deployment may wait without any service converging. It
        /// restarts each time a service converges, so each link of a startup
        /// dependency chain gets the full timeout.
        convergence_timeout_seconds("Convergence timeout (seconds)"): u64 = 120, from 1 to 86_400;
    }

    /// Uploads of local worktrees by `piquelctl preview deploy --local`.
    snapshots("Local snapshots") SnapshotsConfig {
        /// The largest Git bundle one upload may carry, in bytes. Larger uploads
        /// fail with `snapshot_too_large`.
        max_upload_bytes("Largest upload (bytes)"): u64 = 256 * 1024 * 1024, at least 1;
    }

    /// API token lifetimes.
    auth("Authentication") AuthSettings {
        /// The longest lifetime, in days, of new API tokens. When unset, tokens
        /// may have any lifetime, including none. Existing tokens keep theirs.
        max_token_days("Longest token lifetime (days)"): Option<u32> = None, at least 1;
    }
}

/// One daemon setting, as the `settings!` macro declares it.
#[derive(Debug)]
pub struct SettingSpec {
    /// Its key, `section.field`, as the API and `piquelctl settings` name it.
    pub key: &'static str,
    /// The title of its section.
    pub section: &'static str,
    /// A short name for forms and tables.
    pub label: &'static str,
    /// Its doc comment, one source line per line.
    doc: &'static str,
    /// The values it takes.
    pub values: Values,
    /// What it needs of the installation to apply, if anything.
    pub requires: Option<Requirement>,
    /// Whether it keeps history, so that a lower value deletes what a
    /// higher one kept.
    pub prunes: bool,
    /// Reads its value.
    value: fn(&Settings) -> Value,
    /// Sets its value, already [`Self::check`]ed.
    apply: fn(&mut Settings, &Value) -> Result<(), serde_json::Error>,
}

/// Why a setting can't take a value.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SettingError {
    /// No setting has this key.
    #[error("no daemon setting is named {0:?}")]
    Unknown(String),
    /// The installation lacks what the setting needs, so it doesn't apply.
    #[error("{key} applies only with {needs}")]
    Unavailable {
        /// The setting.
        key: &'static str,
        /// What it needs.
        needs: Requirement,
    },
    /// The value is out of the setting's range, or of the wrong type.
    #[error("{key} must be {expected}")]
    Invalid {
        /// The setting.
        key: &'static str,
        /// What it takes, such as `an integer from 1 to 86400`.
        expected: String,
    },
}

impl SettingSpec {
    /// The setting named `key`.
    ///
    /// # Errors
    /// [`SettingError::Unknown`] when no setting has this key.
    pub fn find(key: &str) -> Result<&'static Self, SettingError> {
        SETTINGS
            .iter()
            .find(|spec| spec.key == key)
            .ok_or_else(|| SettingError::Unknown(key.to_owned()))
    }

    /// Its description: its doc comment as prose, a paragraph per line.
    #[must_use]
    pub fn description(&self) -> String {
        self.doc
            .split("\n\n")
            .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The values it takes, as the API reports them.
    #[must_use]
    pub fn kind(&self) -> SettingKind {
        self.values.kind()
    }

    /// Its default value.
    #[must_use]
    pub fn default_value(&self) -> Value {
        self.value_in(&Settings::default())
    }

    /// Its value in `settings`.
    #[must_use]
    pub fn value_in(&self, settings: &Settings) -> Value {
        (self.value)(settings)
    }

    /// Checks that `value` is one this setting takes.
    ///
    /// # Errors
    /// [`SettingError::Invalid`], saying what it takes.
    pub fn check(&self, value: &Value) -> Result<(), SettingError> {
        if self.values.contain(value) {
            return Ok(());
        }
        Err(SettingError::Invalid {
            key: self.key,
            expected: self.kind().to_string(),
        })
    }

    /// Checks `value`, then sets it in `settings`.
    fn apply(&self, settings: &mut Settings, value: &Value) -> Result<(), SettingError> {
        self.check(value)?;
        (self.apply)(settings, value).map_err(|error| SettingError::Invalid {
            key: self.key,
            expected: format!("{}, of its type: {error}", self.kind()),
        })
    }
}

/// Where the effective settings come from: the values set through the API,
/// by key, which the store keeps, over the defaults.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SettingLayers {
    /// The values set through the API, by key. A value this version doesn't
    /// take, such as one a newer version stored before a downgrade, is
    /// ignored.
    pub stored: BTreeMap<String, Value>,
    /// The requirements the installation meets; settings needing another
    /// can't be changed.
    pub met: BTreeSet<Requirement>,
}

impl SettingLayers {
    /// Where `spec`'s effective value comes from, and that value.
    fn layer(&self, spec: &SettingSpec) -> (SettingSource, Value) {
        match self.stored.get(spec.key) {
            Some(stored) if spec.check(stored).is_ok() => (SettingSource::Changed, stored.clone()),
            _ => (SettingSource::Default, spec.default_value()),
        }
    }

    /// The effective settings: each value set through the API, else the
    /// default.
    #[must_use]
    pub fn effective(&self) -> Settings {
        let mut settings = Settings::default();
        for spec in SETTINGS {
            let (_, value) = self.layer(spec);
            let applied = spec.apply(&mut settings, &value);
            debug_assert!(applied.is_ok(), "{}: {applied:?}", spec.key);
        }
        settings
    }

    /// What `spec` needs that the installation lacks, if anything.
    fn unmet(&self, spec: &SettingSpec) -> Option<Requirement> {
        spec.requires.filter(|needs| !self.met.contains(needs))
    }

    /// Checks that `value` may replace `spec`'s, or reset it when `None`:
    /// the setting takes the value, and applies here, unless it is reset,
    /// which a value stored before its requirement went needs.
    ///
    /// # Errors
    /// [`SettingError::Unavailable`] or [`SettingError::Invalid`].
    pub fn check(&self, spec: &SettingSpec, value: Option<&Value>) -> Result<(), SettingError> {
        if let Some(needs) = self.unmet(spec).filter(|_| value.is_some()) {
            return Err(SettingError::Unavailable {
                key: spec.key,
                needs,
            });
        }
        value.map_or(Ok(()), |value| spec.check(value))
    }

    /// `spec` as the API reports it.
    #[must_use]
    pub fn view(&self, spec: &SettingSpec) -> Setting {
        let (source, value) = self.layer(spec);
        Setting {
            key: spec.key.to_owned(),
            section: spec.section.to_owned(),
            label: spec.label.to_owned(),
            description: spec.description(),
            value,
            default: spec.default_value(),
            source,
            kind: spec.kind(),
            needs: self.unmet(spec).map(|needs| needs.to_string()),
            prunes: spec.prunes,
        }
    }

    /// Every setting as the API reports it, in declaration order.
    #[must_use]
    pub fn views(&self) -> Vec<Setting> {
        SETTINGS.iter().map(|spec| self.view(spec)).collect()
    }
}

impl ReconciliationConfig {
    /// The time between full scans.
    #[must_use]
    pub const fn scan_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.scan_interval_seconds)
    }

    /// How long a deployment may take to prepare.
    #[must_use]
    pub const fn prepare_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.prepare_timeout_seconds)
    }

    /// How long a deployment may wait without a service converging.
    #[must_use]
    pub const fn convergence_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.convergence_timeout_seconds)
    }
}
