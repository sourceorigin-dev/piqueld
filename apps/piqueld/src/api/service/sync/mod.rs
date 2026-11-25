//! The sync worker: deploys environments and previews whose branch moved.
//!
//! Every [`TICK`], it lists the branches of each repository that is due, with
//! one `git ls-remote` per repository however many applications, environments
//! and previews follow it, and submits a [`Mutation::Sync`] for each one whose
//! head moved past the head of its last deployment. A repository is due when its
//! poll interval elapsed, when a verified webhook hinted that it moved, while a
//! deployment kept sync from acting on a head, and once after the daemon
//! starts. A listing that fails never deploys anything: the repository is
//! retried with exponential backoff, as it is when a submission fails.
use super::{ApplicationService, ListedHead, Mutation, MutationResponse, SyncOutcome};
use crate::git::Heads;
use futures_util::{StreamExt, stream};
use piqueld_core::{ApplicationId, GitBranch, sync::RepositorySync, sync::SystemActor};
use piqueld_store::{Actor, SyncApplication};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How often the worker looks for due repositories, unless a hint wakes it.
const TICK: Duration = Duration::from_secs(10);
/// Least time between two listings of one application's repository, so a
/// burst of webhooks, or a long deployment, costs one listing per period.
const RECHECK: Duration = Duration::from_secs(30);
/// Delay after a first failed listing, doubled for each further failure.
const BACKOFF: Duration = Duration::from_secs(60);
/// Longest delay between attempts while a repository keeps failing.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Repositories listed at once.
const LISTINGS: usize = 4;

/// Lists a repository's branch heads: `git ls-remote` in the daemon, a
/// fake in tests.
pub(crate) trait Lister: Sync {
    /// Lists every branch of `url` with its head commit.
    fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send;
}

/// Lists branches with `git ls-remote`.
pub(crate) struct Git;

impl Lister for Git {
    fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send {
        Heads::list(url)
    }
}

/// Applications whose repository a verified webhook said moved, waiting for
/// the worker, which they wake.
#[derive(Default)]
pub(crate) struct Hints {
    applications: parking_lot::Mutex<BTreeSet<ApplicationId>>,
    wake: tokio::sync::Notify,
}

impl Hints {
    /// Asks the worker to list `application`'s repository soon.
    pub(crate) fn hint(&self, application: ApplicationId) {
        self.applications.lock().insert(application);
        self.wake.notify_one();
    }

    /// Takes every waiting hint.
    fn take(&self) -> BTreeSet<ApplicationId> {
        std::mem::take(&mut *self.applications.lock())
    }

    /// Puts back `hinted`, taken by a pass that could not act on them, for
    /// the next pass, without waking the worker.
    fn restore(&self, hinted: BTreeSet<ApplicationId>) {
        self.applications.lock().extend(hinted);
    }
}

/// When the worker next lists each repository, by URL, and why. A
/// repository several applications read is listed once for all of them.
#[derive(Default)]
pub(crate) struct Schedule(HashMap<String, Due>);

/// One repository's place in the [`Schedule`].
#[derive(Default)]
struct Due {
    /// When it was last listed successfully.
    checked: Option<Instant>,
    /// Why it must be listed regardless of poll intervals: a webhook, a
    /// deployment that kept sync from acting on a head, or a failure.
    pending: Option<SystemActor>,
    /// Consecutive failures, listing it or submitting what it showed.
    failures: u32,
    /// Not listed before then: `RECHECK` after a listing, or the backoff
    /// after a failure.
    not_before: Option<Instant>,
}

impl Due {
    /// Why the repository must be listed at `now`, if it must, given how the
    /// applications reading it sync. It is polled at the shortest of their
    /// intervals, and listed once after the daemon starts even with only
    /// webhooks, since hints and follow-ups from before then were lost.
    fn trigger(
        &self,
        syncs: impl Iterator<Item = RepositorySync>,
        now: Instant,
    ) -> Option<SystemActor> {
        if self.not_before.is_some_and(|not_before| now < not_before) {
            return None;
        }
        let interval = syncs
            .filter_map(|sync| match sync {
                RepositorySync::Poll { interval_seconds } => {
                    Some(Duration::from_secs(interval_seconds.into()))
                }
                RepositorySync::Off | RepositorySync::Webhook => None,
            })
            .min();
        let polled = self.checked.is_none_or(|checked| {
            interval.is_some_and(|interval| now.duration_since(checked) >= interval)
        });
        self.pending.or(polled.then_some(SystemActor::SyncPoll))
    }

    /// Delays the next attempt after another failure, keeping `trigger`.
    fn failed(&mut self, trigger: SystemActor, now: Instant) {
        self.pending = Some(trigger);
        self.failures = self.failures.saturating_add(1);
        let backoff = BACKOFF
            .saturating_mul(1 << self.failures.saturating_sub(1).min(16))
            .min(MAX_BACKOFF);
        self.not_before = Some(now + backoff);
    }
}

impl ApplicationService {
    /// Runs the sync worker until `cancellation`, which also interrupts a
    /// pass in progress.
    pub(super) async fn sync(&self, cancellation: CancellationToken) {
        let mut schedule = Schedule::default();
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = self.sync_due(&Git, &mut schedule, Instant::now()) => {}
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = self.hints.wake.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Lists every repository due at `now`, at most [`LISTINGS`] at once,
    /// then syncs every application reading each. A failed listing, or a
    /// failure submitting what it showed, records the error on the
    /// applications and backs off.
    pub(crate) async fn sync_due(
        &self,
        lister: &impl Lister,
        schedule: &mut Schedule,
        now: Instant,
    ) {
        // Taken first, so a hint given while applications are read, e.g.
        // by an environment that just started, waits for the next pass,
        // which sees that environment follow its branch.
        let hinted = self.hints.take();
        let applications = match self.store.sync_applications().await {
            Ok(applications) => applications,
            Err(error) => {
                tracing::warn!(?error, "sync could not read applications");
                self.hints.restore(hinted);
                return;
            }
        };
        let mut repositories = BTreeMap::<String, Vec<SyncApplication>>::new();
        for application in applications {
            repositories
                .entry(application.connection.repository.url.clone())
                .or_default()
                .push(application);
        }
        schedule.0.retain(|url, _| repositories.contains_key(url));
        let due = repositories.into_iter().filter_map(|(url, applications)| {
            let state = schedule.0.entry(url.clone()).or_default();
            if applications
                .iter()
                .any(|application| hinted.contains(&application.id))
            {
                state.pending = Some(SystemActor::SyncWebhook);
            }
            let syncs = applications
                .iter()
                .map(|application| application.connection.sync);
            let trigger = state.trigger(syncs, now)?;
            Some((url, applications, trigger))
        });
        let due: Vec<_> = due.collect();
        // Deadlines count from when each repository's turn ended, not from
        // the pass's start, which may be long before with many repositories.
        let started = Instant::now();
        let mut listings = stream::iter(due)
            .map(|(url, applications, trigger)| async move {
                (lister.list(&url).await, url, applications, trigger)
            })
            .buffer_unordered(LISTINGS);
        while let Some((heads, url, applications, trigger)) = listings.next().await {
            let (mut busy, mut failed) = (false, false);
            for application in &applications {
                let result = match &heads {
                    Ok(heads) => self.sync_application(application, heads, trigger).await,
                    Err(error) => Err(format!("{error:#}")),
                };
                let error = result.map(|retry| busy |= retry).err();
                if let Some(error) = &error {
                    tracing::warn!(application = %application.id, %error, "sync failed");
                    failed = true;
                }
                if let Err(error) = self
                    .store
                    .record_sync_check(&application.id, error.as_deref())
                    .await
                {
                    tracing::warn!(?error, "sync could not record its check");
                }
            }
            let (state, done) = (schedule.0.entry(url).or_default(), now + started.elapsed());
            if heads.is_ok() {
                state.checked = Some(done);
            }
            if failed {
                state.failed(trigger, done);
            } else {
                state.failures = 0;
                state.pending = busy.then_some(trigger);
                state.not_before = Some(done + RECHECK);
            }
        }
    }

    /// Submits a [`Mutation::Sync`] by `trigger` for the head in `heads` of
    /// everything following `application`, which deploys it once the branch
    /// moved past the head of its last deployment. A gone branch deploys
    /// nothing. Returns whether a
    /// deployment kept sync from acting on a head, so it tries again, or the
    /// first submission that failed, after trying the others.
    async fn sync_application(
        &self,
        application: &SyncApplication,
        heads: &Heads,
        trigger: SystemActor,
    ) -> Result<bool, String> {
        let (mut busy, mut failure) = (false, None);
        for following in &application.following {
            let Some(branch) = following.source.branch() else {
                continue;
            };
            let Some(head) = heads.head(branch.branch()) else {
                continue;
            };
            let Ok(branch) = GitBranch::parse(branch.branch()) else {
                continue;
            };
            let mutation = Mutation::Sync(ListedHead {
                id: following.id.clone(),
                repository: heads.url().to_owned(),
                branch,
                head: head.to_owned(),
                since: following.synced.clone(),
            });
            match self
                .accept(Actor::System(trigger), mutation, None, false, None)
                .await
            {
                Ok(MutationResponse::Synced(SyncOutcome::Busy)) => busy = true,
                Ok(MutationResponse::Synced(SyncOutcome::Deployed(operation))) => {
                    tracing::info!(environment = %following.id, operation = %operation.operation_id, %head, "sync deployed a moved branch");
                }
                Ok(_) => {}
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        format!("could not deploy {}: {error}", following.name)
                    });
                }
            }
        }
        failure.map_or(Ok(busy), Err)
    }
}

#[cfg(test)]
pub(crate) mod tests;
