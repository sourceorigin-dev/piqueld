//! Secrets: their metadata, who may mount them, and key recovery.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::EnvironmentView;
use crate::manifest::{ApplicationTemplate, SecretSource};
use crate::{EnvironmentId, EnvironmentKind};

/// Metadata only: secret values are never returned.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct SecretMetadata {
    /// Logical name.
    pub name: String,
    /// Current version, used for optimistic writes: a signed 64-bit integer,
    /// unlike the unsigned application and operation generations.
    pub generation: i64,
    /// Last update time in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Cleanup has started; retry deletion to finish it. Replacement is disabled.
    #[serde(default)]
    pub deleting: bool,
    /// The current value was discarded by lost-key recovery; supply a new version.
    #[serde(default)]
    pub unavailable: bool,
}

/// The environments that may mount a stored secret.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentAccess {
    /// Every environment of the application, including ones created later.
    #[default]
    All,
    /// Only these environments, by ID: renaming one keeps its access, and
    /// deleting one removes it from the list.
    Only(BTreeSet<EnvironmentId>),
}

impl EnvironmentAccess {
    /// Query value for `All`. Environment IDs are at least 8 characters, so
    /// it never names one.
    const ALL_QUERY: &str = "all";

    /// Encodes the list as one query parameter: `all`, or comma-separated
    /// environment IDs (empty for none).
    #[must_use]
    pub fn to_query(&self) -> String {
        match self {
            Self::All => Self::ALL_QUERY.to_owned(),
            Self::Only(ids) => ids
                .iter()
                .map(EnvironmentId::as_str)
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// Decodes [`Self::to_query`]'s encoding.
    ///
    /// ```
    /// use piqueld_core::api::EnvironmentAccess;
    /// for access in ["all", "", "env-00000001,env-00000002"] {
    ///     assert_eq!(EnvironmentAccess::from_query(access).unwrap().to_query(), access);
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns the error for a malformed environment ID.
    pub fn from_query(value: &str) -> Result<Self, crate::EnvironmentIdError> {
        if value == Self::ALL_QUERY {
            return Ok(Self::All);
        }
        value
            .split(',')
            .filter(|id| !id.is_empty())
            .map(EnvironmentId::parse)
            .collect::<Result<_, _>>()
            .map(Self::Only)
    }
}

/// Who may mount a secret from an application's store. The default is every
/// environment and no previews.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretAccess {
    /// Environments that may mount the secret.
    pub environments: EnvironmentAccess,
    /// Whether previews may mount the secret. Previews are never covered by
    /// `environments`, even `all`.
    #[serde(default)]
    pub previews: bool,
}

impl SecretAccess {
    /// Whether `environment`, or a preview, may mount the secret.
    #[must_use]
    pub fn allows(&self, environment: &EnvironmentView) -> bool {
        match (&environment.kind, &self.environments) {
            (EnvironmentKind::Preview(_), _) => self.previews,
            (EnvironmentKind::Environment, EnvironmentAccess::All) => true,
            (EnvironmentKind::Environment, EnvironmentAccess::Only(allowed)) => {
                allowed.contains(&environment.id)
            }
        }
    }

    /// The access in words, naming environments by their current name:
    /// `every environment`, `production, staging and previews`, `no environment`.
    #[must_use]
    pub fn describe(&self, environments: &[EnvironmentView]) -> String {
        let named = match &self.environments {
            EnvironmentAccess::All => "every environment".to_owned(),
            EnvironmentAccess::Only(allowed) if allowed.is_empty() => "no environment".to_owned(),
            EnvironmentAccess::Only(allowed) => allowed
                .iter()
                .map(|id| {
                    environments
                        .iter()
                        .find(|environment| environment.id == *id)
                        .map_or_else(
                            || id.to_string(),
                            |environment| environment.name.to_string(),
                        )
                })
                .collect::<Vec<_>>()
                .join(", "),
        };
        if self.previews {
            format!("{named} and previews")
        } else {
            named
        }
    }
}

/// A stored secret's new access list, optionally guarded by the one the
/// caller read, so concurrent changes aren't lost: the change fails with 409
/// `secret_access_conflict` once the secret's access is no longer `expected`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretAccessChange {
    /// Environments that may mount the secret.
    pub environments: EnvironmentAccess,
    /// Whether previews may mount the secret. Previews are never covered by
    /// `environments`, even `all`.
    #[serde(default)]
    pub previews: bool,
    /// The access the change was made from; without it, the change replaces
    /// whatever access is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<SecretAccess>,
}

impl SecretAccessChange {
    /// Changes the access to `access`, guarded by `expected` when given.
    #[must_use]
    pub fn new(access: SecretAccess, expected: Option<SecretAccess>) -> Self {
        Self {
            environments: access.environments,
            previews: access.previews,
            expected,
        }
    }

    /// The new access, and the one it was made from.
    #[must_use]
    pub fn into_parts(self) -> (SecretAccess, Option<SecretAccess>) {
        let access = SecretAccess {
            environments: self.environments,
            previews: self.previews,
        };
        (access, self.expected)
    }
}

/// A manually set secret in an application's store: its metadata and access,
/// never its value.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct StoredSecret {
    /// Version metadata.
    #[serde(flatten)]
    pub metadata: SecretMetadata,
    /// Who may mount the secret.
    pub access: SecretAccess,
}

/// Where the value of a secret an environment mounts comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MountedSecret {
    /// Generated for the environment, because `spec.secrets` declares it.
    Generated,
    /// The application store's secret, which the environment may mount.
    Stored {
        /// Current version.
        generation: i64,
    },
    /// Neither declared nor set in the application store: deploying fails.
    Missing,
    /// The application store's secret, whose access list excludes the
    /// environment: deploying fails with `secret_access_denied`.
    Denied,
    /// The application store's secret, whose current value key recovery
    /// discarded: deploying fails with `secret_unavailable` until it is replaced.
    Unavailable,
}

impl MountedSecret {
    /// Each secret `template`, the manifest `environment` deploys, mounts
    /// there, given the application's `stored` secrets.
    #[must_use]
    pub fn list(
        template: &ApplicationTemplate,
        environment: &EnvironmentView,
        stored: &[StoredSecret],
    ) -> Vec<(String, Self)> {
        template
            .mounted_secrets(&environment.target())
            .into_iter()
            .map(|(name, source)| {
                let mounted = match source {
                    SecretSource::Generated => Self::Generated,
                    SecretSource::Stored => {
                        match stored.iter().find(|secret| secret.metadata.name == name) {
                            None => Self::Missing,
                            Some(secret) if !secret.access.allows(environment) => Self::Denied,
                            Some(secret) if secret.metadata.unavailable => Self::Unavailable,
                            Some(secret) => Self::Stored {
                                generation: secret.metadata.generation,
                            },
                        }
                    }
                };
                (name, mounted)
            })
            .collect()
    }
}

impl std::fmt::Display for MountedSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generated => formatter.write_str("generated for this environment"),
            Self::Stored { generation } => {
                write!(formatter, "application store, version {generation}")
            }
            Self::Missing => formatter.write_str("not set in the application store"),
            Self::Denied => formatter.write_str("application store, not allowed here"),
            Self::Unavailable => {
                formatter.write_str("application store, value discarded; replace it")
            }
        }
    }
}

/// Metadata-only result of lost-key recovery. Never rotates application credentials.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct SecretKeyRecovery {
    /// Environments whose generated values were discarded.
    pub affected_environments: i64,
    /// Applications whose stored values were discarded.
    #[serde(default)]
    pub affected_applications: i64,
    /// Logical secrets, stored and generated, whose values were discarded.
    pub affected_secrets: i64,
    /// Stored and generated versions discarded; already unavailable versions
    /// are excluded.
    pub discarded_versions: i64,
    /// Notification destinations deleted, since their URLs were encrypted.
    #[serde(default)]
    pub deleted_destinations: i64,
}
