//! Transport-independent logic of the dashboard's deployment and preview
//! views: deployment history paging, what a deployment's row says,
//! attempt lists, promotion confirmation, and preview creation.

use piqueld_client::{
    CreatePreviewRequest, DeploymentOrigin, DeploymentSummary, DeploymentView, Operation,
    OperationKind, Page, PlanView, PromoteRequest, ReleaseAvailability, SystemVariable,
    sync::short_commit,
};

/// What a deployment's row in its environment's history, and its page, say
/// of it: from its summary or its whole snapshot.
pub trait DeploymentFacts {
    /// The deployment's operation.
    fn operation(&self) -> &Operation;

    /// The commit and the branch its manifest was read from; `None` for a
    /// saved manifest.
    fn source(&self) -> (Option<&str>, Option<&str>);

    /// The branch and commit its manifest was read from, such as
    /// "main @ 0123456789ab"; `None` for a saved manifest.
    fn commit(&self) -> Option<String> {
        let (sha, branch) = self.source();
        let commit = short_commit(sha?);
        Some(match branch {
            Some(branch) => format!("{branch} @ {commit}"),
            None => commit.to_owned(),
        })
    }

    /// How long it ran once it ended, from its start, or creation if it
    /// never started.
    fn duration_ms(&self) -> Option<u64> {
        let operation = self.operation();
        let started = operation.started_at_ms.unwrap_or(operation.created_at_ms);
        u64::try_from(operation.finished_at_ms? - started).ok()
    }

    /// Whether `latest`, its environment's latest operation, is a newer
    /// deployment. The daemon picks the latest by creation time: the
    /// deployment list, paged by ID, can list an older deployment first.
    fn superseded_by(&self, latest: Option<&Operation>) -> bool {
        let operation = self.operation();
        latest.is_some_and(|latest| {
            latest.kind != OperationKind::Delete
                && latest.environment_id == operation.environment_id
                && latest.id != operation.id
        })
    }
}

impl DeploymentFacts for DeploymentView {
    fn operation(&self) -> &Operation {
        &self.operation
    }

    fn source(&self) -> (Option<&str>, Option<&str>) {
        (
            self.system_value(SystemVariable::GitSha),
            self.system_value(SystemVariable::GitBranch),
        )
    }
}

impl DeploymentFacts for DeploymentSummary {
    fn operation(&self) -> &Operation {
        &self.operation
    }

    fn source(&self) -> (Option<&str>, Option<&str>) {
        (self.git_sha.as_deref(), self.git_branch.as_deref())
    }
}

/// The deployments of one environment shown in its history, newest first:
/// its newest page, polled, and older pages appended on request.
pub trait DeploymentList {
    /// Merges a freshly polled newest page into a list that also holds older
    /// pages: deployments are replaced or added by operation ID, keeping the
    /// older ones, and the `current_target` and `last_successful` flags are
    /// cleared on listed deployments when the page carries them, so only one
    /// deployment holds each flag.
    fn merge_newest(&mut self, page: Vec<DeploymentSummary>);

    /// Appends an older page, skipping deployments already listed.
    fn append_older(&mut self, page: Vec<DeploymentSummary>);
}

impl DeploymentList for Vec<DeploymentSummary> {
    fn merge_newest(&mut self, page: Vec<DeploymentSummary>) {
        let has_current = page.iter().any(|item| item.current_target);
        let has_successful = page.iter().any(|item| item.last_successful);
        for item in self.iter_mut() {
            item.current_target &= !has_current;
            item.last_successful &= !has_successful;
        }
        for item in page {
            match self
                .iter_mut()
                .find(|old| old.operation.id == item.operation.id)
            {
                Some(old) => *old = item,
                None => self.push(item),
            }
        }
        self.sort_by(|a, b| b.operation.id.cmp(&a.operation.id));
    }

    fn append_older(&mut self, page: Vec<DeploymentSummary>) {
        for item in page {
            if !self.iter().any(|old| old.operation.id == item.operation.id) {
                self.push(item);
            }
        }
    }
}

/// The retained attempt outcomes of one deployment loaded so far, newest
/// first, and the cursor of the next older page.
#[derive(Clone, Debug, Default)]
pub struct Attempts {
    items: Vec<Operation>,
    /// Cursor of the next older page, if any.
    pub cursor: Option<String>,
}

impl Attempts {
    /// Shows `page`: the newest attempts, replacing those shown, or with
    /// `older` the attempts following them.
    pub fn show(&mut self, page: Page<Operation>, older: bool) {
        if older {
            self.items.extend(page.items);
        } else {
            self.items = page.items;
        }
        self.cursor = page.next_cursor;
    }

    /// The attempts to list for a deployment whose operation is `live`; see
    /// [`Operation::attempts`].
    #[must_use]
    pub fn visible(&self, live: &Operation) -> Vec<Operation> {
        live.attempts(&self.items)
    }
}

/// What a promotion into an environment deploys: its source's current
/// deployment, or an earlier `release`.
#[derive(Clone, Debug)]
pub struct Promotion {
    release: Option<String>,
}

impl Promotion {
    /// Promotes the source's current deployment, or deploys `release`.
    #[must_use]
    pub const fn new(release: Option<String>) -> Self {
        Self { release }
    }

    /// "Deploy" for an earlier release, else "Promote".
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        if self.release.is_some() {
            "Deploy"
        } else {
            "Promote"
        }
    }

    /// The request planning this promotion.
    #[must_use]
    pub fn request(&self) -> PromoteRequest {
        PromoteRequest {
            release: self.release.clone(),
            ..PromoteRequest::default()
        }
    }

    /// The request promoting exactly what `plan` reviewed: pinned to the
    /// source deployment it planned and the application revision it was
    /// planned at. None when the plan has no release, or the release mounts
    /// secrets the environment lacks or may not use, or its images are gone.
    #[must_use]
    pub fn confirmed(&self, plan: &PlanView) -> Option<PromoteRequest> {
        let release = plan.release.as_ref()?;
        if !release.secrets.is_empty()
            || matches!(
                release.release.availability,
                Some(ReleaseAvailability::Unavailable { .. })
            )
        {
            return None;
        }
        let mut request = self.request();
        if let DeploymentOrigin::Promotion { deployment, .. } = &release.origin {
            request.deployment = Some(deployment.clone());
        }
        request.expected_generation = Some(plan.generation);
        Some(request)
    }
}

/// The "Create preview" form: a branch, and an optional slot telling apart
/// several previews of it.
#[derive(Clone, Debug, Default)]
pub struct PreviewForm {
    /// Branch to deploy.
    pub branch: String,
    /// Slot, blank for none.
    pub slot: String,
}

impl PreviewForm {
    /// The request creating the preview, trimmed; a blank slot is none.
    #[must_use]
    pub fn request(&self) -> CreatePreviewRequest {
        CreatePreviewRequest {
            branch: self.branch.trim().to_owned(),
            slot: Some(self.slot.trim().to_owned()).filter(|slot| !slot.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests;
