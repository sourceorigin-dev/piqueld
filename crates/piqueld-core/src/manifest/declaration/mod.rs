//! Environments an application's manifest declares, and how its environments
//! differ from them.
//!
//! Every `[spec.environments.<name>]` block declares the environment `<name>`
//! and where it deploys from. A manifest that declares environments is the
//! source of truth for which environments exist and how each is sourced:
//! saving it, or fetching it from the branch `spec.manifest` names, creates the
//! missing ones and changes the sources of the others. Nothing is deployed, and
//! nothing is ever deleted: environments it no longer declares are reported
//! until deleted explicitly. A manifest that declares none leaves environments
//! to `piquelctl env`, as before.

use super::{ApplicationSpec, EnvironmentConfig, ValidationError, ValidationErrors};
use crate::api::EnvironmentView;
use crate::{ApiErrorCode, EnvironmentName, EnvironmentSource, TrackedBranch};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Where a declared environment deploys from, naming other environments by
/// name, as the manifest does.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeclaredSource {
    /// The application's saved manifest.
    Saved,
    /// The manifest file on a branch of the application's manifest repository.
    Branch(TrackedBranch),
    /// Releases promoted from another environment of the application.
    Promoted {
        /// The environment it receives releases from.
        from: EnvironmentName,
    },
}

/// Describes the source in a few words, as [`EnvironmentSource`] does.
///
/// ```text
/// the saved manifest    branch main    branch main@0123…    releases promoted from staging
/// ```
impl std::fmt::Display for DeclaredSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saved => formatter.write_str("the saved manifest"),
            Self::Branch(branch) => write!(formatter, "branch {branch}"),
            Self::Promoted { from } => write!(formatter, "releases promoted from {from}"),
        }
    }
}

/// One environment as the manifest declares it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    /// The environment's name, the block's key.
    name: EnvironmentName,
    /// Where it deploys from.
    source: DeclaredSource,
    /// Whether pushes to its branch deploy it while the repository syncs.
    /// Always false for promoted environments, which never build.
    sync: bool,
    /// Whether converging changes an existing environment's source and sync
    /// to these: its block sets them, or the previous manifest's did, so that
    /// removing them returns it to the default. Otherwise they only create a
    /// missing environment, and an existing one keeps the source it has, such
    /// as one set with `piquelctl env source` before blocks declared sources.
    manages_source: bool,
}

impl Declaration {
    /// The environment a new application with `declarations` deploys first:
    /// the first declared one that builds, or `production`.
    #[must_use]
    pub fn first_building(declarations: &[Self]) -> EnvironmentName {
        declarations
            .iter()
            .find(|declaration| !matches!(declaration.source, DeclaredSource::Promoted { .. }))
            .map_or_else(EnvironmentName::default_name, |declaration| {
                declaration.name.clone()
            })
    }
}

impl ApplicationSpec {
    /// The environments this manifest declares, each promotion source before
    /// the environments promoting from it. Tracking environments deploy the
    /// saved manifest or follow a branch of `spec.manifest`'s repository.
    ///
    /// # Errors
    ///
    /// Returns every problem at its field path: invalid names, branches and
    /// commits; a source on a promoted environment; `branch`, `commit` or
    /// `sync` without a manifest repository; `sync` with its own `commit`; and
    /// a `promote_from` naming an undeclared environment or leading back to
    /// itself.
    pub fn declared_environments(&self) -> Result<Vec<Declaration>, ValidationErrors> {
        self.declared_environments_since(None)
    }

    /// [`Self::declared_environments`] when this manifest replaces `previous`:
    /// sources `previous` set and this one removed are managed too, so they
    /// return to the default.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::declared_environments`].
    pub fn declared_environments_since(
        &self,
        previous: Option<&Self>,
    ) -> Result<Vec<Declaration>, ValidationErrors> {
        let mut errors = Vec::new();
        let mut declared = BTreeMap::new();
        for (name, config) in &self.environments {
            let path = format!("spec.environments.{name}");
            match EnvironmentName::parse(name.as_str()) {
                Ok(parsed) => match self.declare(parsed, config, &path) {
                    Ok(mut declaration) => {
                        declaration.manages_source |= previous
                            .and_then(|previous| previous.environments.get(name))
                            .is_some_and(EnvironmentConfig::sets_source);
                        declared.insert(name.as_str(), declaration);
                    }
                    Err(invalid) => errors.extend(invalid),
                },
                Err(source) => errors.push(ValidationError::new(
                    ApiErrorCode::NameInvalid,
                    "spec.environments",
                    format!(
                        "[spec.environments.{}]: {source}",
                        super::variables::echo(name)
                    ),
                )),
            }
        }
        let mut ordered = Vec::with_capacity(declared.len());
        let mut placed = std::collections::BTreeSet::new();
        for name in declared.keys() {
            if placed.contains(name) {
                continue;
            }
            let mut chain = vec![*name];
            while let Some(DeclaredSource::Promoted { from }) = declared
                .get(chain[chain.len() - 1])
                .map(|declaration| &declaration.source)
            {
                let from = from.as_str();
                if placed.contains(from) {
                    break;
                }
                if !declared.contains_key(from) {
                    if self.environments.contains_key(from) {
                        break;
                    }
                    errors.push(ValidationError::new(
                        ApiErrorCode::PromotionSourceInvalid,
                        format!("spec.environments.{}.promote_from", chain[chain.len() - 1]),
                        format!(
                            "{from} is not declared: promotion sources are environments of spec.environments"
                        ),
                    ));
                    break;
                }
                if let Some(start) = chain.iter().position(|name| *name == from) {
                    let mut cycle = chain[start..].to_vec();
                    cycle.push(from);
                    errors.push(ValidationError::new(
                        ApiErrorCode::PromotionCycle,
                        format!("spec.environments.{}.promote_from", chain[chain.len() - 1]),
                        format!(
                            "an environment cannot promote from itself: {}",
                            cycle.join(" ← ")
                        ),
                    ));
                    break;
                }
                chain.push(from);
            }
            for name in chain.into_iter().rev() {
                if placed.insert(name) {
                    ordered.push(declared[name].clone());
                }
            }
        }
        if errors.is_empty() {
            Ok(ordered)
        } else {
            Err(ValidationErrors::sorted(errors))
        }
    }

    /// The declaration of environment `name` by `config`, whose block is at
    /// `path`, or the errors at its fields.
    #[expect(
        clippy::map_err_ignore,
        reason = "the diagnostic names the field and says why it is invalid"
    )]
    fn declare(
        &self,
        name: EnvironmentName,
        config: &EnvironmentConfig,
        path: &str,
    ) -> Result<Declaration, Vec<ValidationError>> {
        let fail = |field: &str, code: ApiErrorCode, message: String| {
            ValidationError::new(code, format!("{path}.{field}"), message)
        };
        if let Some(from) = &config.promote_from {
            if config.branch.is_some() || config.commit.is_some() || config.sync.is_some() {
                return Err(vec![fail(
                    "promote_from",
                    ApiErrorCode::EnvironmentSourceConflict,
                    "a promoted environment never builds: remove branch, commit and sync".into(),
                )]);
            }
            let from = EnvironmentName::parse(from.as_str()).map_err(|_| {
                vec![fail(
                    "promote_from",
                    ApiErrorCode::PromotionSourceInvalid,
                    format!(
                        "{} is not an environment name",
                        super::variables::echo(from)
                    ),
                )]
            })?;
            return Ok(Declaration {
                name,
                source: DeclaredSource::Promoted { from },
                sync: false,
                manages_source: true,
            });
        }
        let Some(connection) = &self.manifest else {
            let errors = [
                ("branch", config.branch.is_some()),
                ("commit", config.commit.is_some()),
                ("sync", config.sync == Some(true)),
            ]
            .into_iter()
            .filter(|(_, set)| *set)
            .map(|(field, _)| {
                fail(
                    field,
                    ApiErrorCode::ManifestRepositoryRequired,
                    "only environments of a repository-backed application follow a branch; add spec.manifest first".into(),
                )
            })
            .collect::<Vec<_>>();
            return if errors.is_empty() {
                Ok(Declaration {
                    name,
                    source: DeclaredSource::Saved,
                    sync: false,
                    manages_source: config.sets_source(),
                })
            } else {
                Err(errors)
            };
        };
        let (branch, commit) = match &config.branch {
            Some(branch) => (branch.clone(), config.commit.clone()),
            None => (
                connection.repository.branch.clone(),
                config
                    .commit
                    .clone()
                    .or_else(|| connection.repository.commit.clone()),
            ),
        };
        let branch = TrackedBranch::new(branch, commit).map_err(|invalid| {
            invalid
                .0
                .into_iter()
                .filter(|error| match error.path.as_str() {
                    "branch" => config.branch.is_some(),
                    _ => config.commit.is_some(),
                })
                .map(|error| ValidationError {
                    path: format!("{path}.{}", error.path),
                    ..error
                })
                .collect::<Vec<_>>()
        })?;
        let sync = config.sync.unwrap_or(false);
        if sync && config.commit.is_some() {
            return Err(vec![fail(
                "sync",
                ApiErrorCode::EnvironmentSyncPinned,
                "pinned environments never sync: remove commit, or sync".into(),
            )]);
        }
        Ok(Declaration {
            name,
            source: DeclaredSource::Branch(branch),
            sync,
            manages_source: config.sets_source(),
        })
    }

    /// What saving this manifest, which declares `declarations`, changes in
    /// an application's `environments`, or creates for a new application
    /// (`None`): one that declares none gets `production`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`TrackedBranch::of`] for `spec.manifest`.
    pub fn environment_changes(
        &self,
        declarations: &[Declaration],
        environments: Option<&[EnvironmentView]>,
    ) -> Result<Vec<EnvironmentChange>, ValidationErrors> {
        Ok(match environments {
            Some(environments) => EnvironmentChange::between(declarations, environments),
            None if declarations.is_empty() => vec![EnvironmentChange::Create {
                environment: EnvironmentName::default_name(),
                source: match &self.manifest {
                    Some(connection) => {
                        DeclaredSource::Branch(TrackedBranch::of(&connection.repository)?)
                    }
                    None => DeclaredSource::Saved,
                },
                sync: false,
            }],
            None => EnvironmentChange::between(declarations, &[]),
        })
    }
}

/// A difference between the environments a manifest declares and the
/// application's environments, as converging them would resolve it, or, for
/// `undeclared`, report it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum EnvironmentChange {
    /// The environment is created, not deployed.
    Create {
        /// The declared environment.
        environment: EnvironmentName,
        /// Where it deploys from.
        source: DeclaredSource,
        /// Whether pushes deploy it, from its first deployment.
        sync: bool,
    },
    /// The environment deploys from `to` instead of `from`; nothing is
    /// deployed. Waits while a deployment of it runs when it becomes promoted.
    Source {
        /// The declared environment.
        environment: EnvironmentName,
        /// Where it deploys from now.
        from: DeclaredSource,
        /// Where it is declared to deploy from.
        to: DeclaredSource,
    },
    /// Pushes start or stop deploying the environment.
    Sync {
        /// The declared environment.
        environment: EnvironmentName,
        /// Whether pushes deploy it.
        sync: bool,
    },
    /// The manifest no longer declares the environment. It keeps running,
    /// with its volumes and secrets, until deleted explicitly.
    Undeclared {
        /// The environment.
        environment: EnvironmentName,
    },
}

impl EnvironmentChange {
    /// What converging `environments` (never previews) to `declarations`
    /// changes, in the order of `declarations`, then the environments it
    /// leaves undeclared. Existing environments change only where their
    /// declaration manages their source (see [`Declaration::manages_source`]).
    /// Without declarations there is nothing to converge. Environments being
    /// deleted are gone: a declaration of one's name is created once it is.
    #[must_use]
    pub fn between(declarations: &[Declaration], environments: &[EnvironmentView]) -> Vec<Self> {
        if declarations.is_empty() {
            return Vec::new();
        }
        let live = environments
            .iter()
            .filter(|environment| !environment.delete_intent && environment.preview().is_none())
            .collect::<Vec<_>>();
        let mut changes = Vec::new();
        for declaration in declarations {
            let environment = declaration.name.clone();
            let Some(current) = live.iter().find(|current| current.name == environment) else {
                changes.push(Self::Create {
                    environment,
                    source: declaration.source.clone(),
                    sync: declaration.sync,
                });
                continue;
            };
            if !declaration.manages_source {
                continue;
            }
            let from = Self::named(&current.source, environments);
            if let Some(from) = from.filter(|from| *from != declaration.source) {
                changes.push(Self::Source {
                    environment: environment.clone(),
                    from,
                    to: declaration.source.clone(),
                });
            }
            let promoted = matches!(declaration.source, DeclaredSource::Promoted { .. });
            if !promoted && current.sync != declaration.sync {
                changes.push(Self::Sync {
                    environment,
                    sync: declaration.sync,
                });
            }
        }
        changes.extend(
            live.iter()
                .filter(|current| {
                    !declarations
                        .iter()
                        .any(|declaration| declaration.name == current.name)
                })
                .map(|current| Self::Undeclared {
                    environment: current.name.clone(),
                }),
        );
        changes
    }

    /// `source`, naming its promotion source among `environments`, which
    /// always has it: sources are environments of the same application, and
    /// can't be deleted while environments promote from them.
    fn named(
        source: &EnvironmentSource,
        environments: &[EnvironmentView],
    ) -> Option<DeclaredSource> {
        Some(match source {
            EnvironmentSource::Saved => DeclaredSource::Saved,
            EnvironmentSource::Branch(branch) => DeclaredSource::Branch(branch.clone()),
            EnvironmentSource::Promoted(from) => DeclaredSource::Promoted {
                from: environments
                    .iter()
                    .find(|environment| environment.id == from.environment)?
                    .name
                    .clone(),
            },
        })
    }

    /// The environment it concerns.
    #[must_use]
    pub const fn environment(&self) -> &EnvironmentName {
        match self {
            Self::Create { environment, .. }
            | Self::Source { environment, .. }
            | Self::Sync { environment, .. }
            | Self::Undeclared { environment } => environment,
        }
    }

    /// Describes the change, without the environment's name.
    ///
    /// ```text
    /// created, deploys from branch main, follows pushes
    /// deploys from releases promoted from staging instead of branch main
    /// no longer declared; kept until deleted with `piquelctl env delete`
    /// ```
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Create {
                source,
                sync: false,
                ..
            } => format!("created, deploys from {source}"),
            Self::Create {
                source, sync: true, ..
            } => format!("created, deploys from {source}, follows pushes"),
            Self::Source { from, to, .. } => format!("deploys from {to} instead of {from}"),
            Self::Sync { sync: true, .. } => "follows pushes".into(),
            Self::Sync { sync: false, .. } => "no longer follows pushes".into(),
            Self::Undeclared { .. } => {
                "no longer declared; kept until deleted with `piquelctl env delete`".into()
            }
        }
    }
}

/// The environment's name, then [`EnvironmentChange::summary`].
impl std::fmt::Display for EnvironmentChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.environment(), self.summary())
    }
}

#[cfg(test)]
mod tests;
