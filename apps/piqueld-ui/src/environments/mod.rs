//! Changes the dashboard makes to an application's environments, and the
//! source form that chooses where an environment deploys from: building its
//! own source (the saved manifest, or a branch of the application's
//! repository) or receiving releases promoted from another environment.

use piqueld_client::{
    ApplicationSpec, ApplicationView, Client, ClientError, CreateEnvironmentRequest,
    EnvironmentBranchRequest, EnvironmentId, EnvironmentRequest, EnvironmentSource,
    EnvironmentSourceRequest, EnvironmentView, TrackedBranch,
};

/// Dirty group of the environment page's source form.
pub const SOURCE_FORM: &str = "environment-source";
/// Dirty group of the environment page's rename form.
pub const NAME_FORM: &str = "environment-name";

/// A change to one of the application's environments. All but `Sync` are
/// guarded by the saved generation.
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentEdit {
    /// Creates an environment.
    Create {
        /// The new environment's name.
        name: String,
        /// The branch it follows, in a repository-backed application.
        branch: Option<TrackedBranch>,
        /// The environment it is promoted from; excludes `branch`.
        promote_from: Option<String>,
    },
    /// Renames environment `id`.
    Rename {
        /// The environment.
        id: String,
        /// Its new name.
        name: String,
    },
    /// Makes environment `id` follow `branch`.
    Branch {
        /// The environment.
        id: String,
        /// The branch, and the commit it is pinned to, if any.
        branch: TrackedBranch,
    },
    /// Opts in or out of the application's sync, without a revision.
    Sync {
        /// The environment.
        id: String,
        /// Whether pushes deploy it.
        enabled: bool,
    },
    /// Grants or revokes the daemon's telemetry, without a revision.
    Telemetry {
        /// The environment.
        id: String,
        /// Whether it receives the telemetry.
        granted: bool,
    },
    /// Promoted from the given environment, or `None` to track its own source.
    Source {
        /// The environment.
        id: String,
        /// The environment it receives releases from.
        promote_from: Option<String>,
    },
    /// Deletes the environment, retaining its volumes.
    Delete(String),
    /// Retries a deletion that did not finish.
    RetryDeletion(String),
}

impl EnvironmentEdit {
    /// Whether a replay after a transport failure is safe. Deletions are not
    /// retried, as they could fail once the first attempt completed.
    #[must_use]
    pub const fn replayable(&self) -> bool {
        !matches!(self, Self::Delete(_) | Self::RetryDeletion(_))
    }

    /// The form whose unsaved edits the change saves, if any.
    #[must_use]
    pub const fn form(&self) -> Option<&'static str> {
        match self {
            Self::Rename { .. } => Some(NAME_FORM),
            Self::Branch { .. } | Self::Source { .. } => Some(SOURCE_FORM),
            _ => None,
        }
    }

    /// Whether the change advances the application revision, which later
    /// guarded edits must then expect.
    const fn advances(&self) -> bool {
        !matches!(
            self,
            Self::RetryDeletion(_) | Self::Sync { .. } | Self::Telemetry { .. }
        )
    }

    /// Records the applied change in `saved`: the `changed` environment the
    /// daemon returned replaces its previous view, in name order, or without
    /// one a deleted environment is marked as being deleted.
    pub fn apply(&self, saved: &mut ApplicationView, changed: Option<EnvironmentView>) {
        if self.advances() {
            saved.generation += 1;
        }
        let environments = &mut saved.environments;
        if let Some(changed) = changed {
            environments.retain(|env| env.id != changed.id);
            environments.push(changed);
            environments.sort_by(|a, b| a.name.cmp(&b.name));
        } else if let Self::Delete(id) = self
            && let Some(env) = environments.iter_mut().find(|env| env.id.as_str() == id)
        {
            env.delete_intent = true;
        }
    }

    /// Sends the change, returning the changed environment unless it is
    /// being deleted.
    ///
    /// # Errors
    ///
    /// Returns the request's transport or API error.
    pub async fn send(
        &self,
        client: &Client,
        application: &str,
        generation: u64,
    ) -> Result<Option<EnvironmentView>, ClientError> {
        let expected_generation = Some(generation);
        match self {
            Self::Create {
                name,
                branch,
                promote_from,
            } => {
                let request = CreateEnvironmentRequest {
                    name: name.clone(),
                    branch: branch.as_ref().map(|branch| branch.branch().to_owned()),
                    commit: branch
                        .as_ref()
                        .and_then(|branch| branch.commit().map(str::to_owned)),
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .create_environment(application, &request, false)
                    .await
                    .map(Some)
            }
            Self::Rename { id, name } => {
                let request = EnvironmentRequest {
                    name: name.clone(),
                    expected_generation,
                };
                client
                    .rename_environment(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Branch { id, branch } => {
                let request = EnvironmentBranchRequest {
                    branch: branch.branch().to_owned(),
                    commit: branch.commit().map(str::to_owned),
                    expected_generation,
                };
                client
                    .set_environment_branch(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Sync { id, enabled } => client.set_environment_sync(id, *enabled).await.map(Some),
            Self::Telemetry { id, granted } => client
                .set_environment_telemetry(id, *granted)
                .await
                .map(Some),
            Self::Source { id, promote_from } => {
                let request = EnvironmentSourceRequest {
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .set_environment_source(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Delete(id) => client
                .delete_environment(id, Some(generation), false)
                .await
                .map(|_| None),
            Self::RetryDeletion(id) => client
                .reconcile_environment(id, Some(generation))
                .await
                .map(|_| None),
        }
    }
}

/// A branch and optional commit as typed into a form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchDraft {
    /// The branch name.
    pub branch: String,
    /// Blank to follow the branch head.
    pub commit: String,
}

impl BranchDraft {
    /// The branch and commit new environments of an application with `spec`
    /// follow by default: those its `spec.manifest` names, or blank when it
    /// has no repository.
    #[must_use]
    pub fn default_for(spec: &ApplicationSpec) -> Self {
        spec.manifest.as_ref().map_or_default(|manifest| Self {
            branch: manifest.repository.branch.clone(),
            commit: manifest.repository.commit.clone().unwrap_or_default(),
        })
    }

    /// Validates the draft, ignoring surrounding whitespace; a blank commit
    /// follows the branch head.
    ///
    /// # Errors
    ///
    /// Returns the validation message of an invalid branch or commit.
    pub fn tracked(&self) -> Result<TrackedBranch, String> {
        let commit = self.commit.trim();
        let commit = (!commit.is_empty()).then(|| commit.to_owned());
        TrackedBranch::new(self.branch.trim().to_owned(), commit).map_err(|error| error.to_string())
    }
}

impl From<&TrackedBranch> for BranchDraft {
    fn from(branch: &TrackedBranch) -> Self {
        Self {
            branch: branch.branch().to_owned(),
            commit: branch.commit().unwrap_or_default().to_owned(),
        }
    }
}

/// What the environment page's Source form shows: the branch it builds and
/// the environment it is promoted from, empty to build its own source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceDraft {
    /// The branch it builds, shown while it builds its own source in a
    /// repository-backed application.
    pub branch: BranchDraft,
    /// The environment it is promoted from; empty to build its own source.
    pub promote_from: String,
}

impl SourceDraft {
    /// The form showing `source` as saved; one that follows no branch shows
    /// `default`, the branch it would build.
    #[must_use]
    pub fn saved(source: Option<&EnvironmentSource>, default: BranchDraft) -> Self {
        Self {
            branch: source
                .and_then(EnvironmentSource::branch)
                .map_or(default, BranchDraft::from),
            promote_from: source
                .and_then(EnvironmentSource::promoted_from)
                .map_or_default(ToString::to_string),
        }
    }

    /// Whether the form chooses building its own source over promotion.
    #[must_use]
    pub fn builds(&self) -> bool {
        self.promote_from.is_empty()
    }

    /// The change saving the form makes to environment `id`, whose `saved`
    /// source it edits, in an application that is `repository`-backed or
    /// deploys its saved manifest; `None` when it shows the saved source.
    /// Only repository-backed environments follow a branch.
    #[must_use]
    pub fn change(
        &self,
        id: String,
        saved: Option<&EnvironmentSource>,
        repository: bool,
    ) -> Option<Result<EnvironmentEdit, String>> {
        let promoted = saved.and_then(EnvironmentSource::promoted_from);
        if !self.builds() {
            return (promoted.map(EnvironmentId::as_str) != Some(self.promote_from.as_str())).then(
                || {
                    Ok(EnvironmentEdit::Source {
                        id,
                        promote_from: Some(self.promote_from.clone()),
                    })
                },
            );
        }
        if repository {
            return match self.branch.tracked() {
                Ok(tracked) if saved.and_then(EnvironmentSource::branch) == Some(&tracked) => None,
                Ok(branch) => Some(Ok(EnvironmentEdit::Branch { id, branch })),
                Err(error) => Some(Err(error)),
            };
        }
        promoted.map(|_| {
            Ok(EnvironmentEdit::Source {
                id,
                promote_from: None,
            })
        })
    }
}

#[cfg(test)]
mod tests;
