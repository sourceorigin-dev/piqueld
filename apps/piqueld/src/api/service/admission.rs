//! Admission of expensive API work: each kind runs a bounded number of times
//! at once across every listener, and more is refused at once rather than
//! queued.
use super::{ApplicationError, ApplicationService};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Kinds of expensive work, each with its own budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expensive {
    /// Deployment analytics or traffic, aggregated over a time window.
    Analytics,
    /// Recent container output, read from Docker.
    Logs,
    /// Commands run in a container, for as long as they run.
    Exec,
    /// Event streams, for as long as they stay open.
    EventStream,
    /// Live log streams, which follow each container's output from Docker,
    /// for as long as they stay open.
    LogStream,
    /// Installation checks, which probe the network and Docker.
    Checks,
}
impl Expensive {
    /// How many of this kind may run at once.
    const fn limit(self) -> usize {
        match self {
            Self::Analytics => 4,
            Self::Logs => 8,
            Self::Exec => 32,
            Self::EventStream => 128,
            Self::LogStream => 16,
            Self::Checks => 2,
        }
    }

    /// A fresh budget for this kind.
    fn budget(self) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(self.limit()))
    }
}

/// One budget per [`Expensive`] kind, shared by clones of the service.
pub(super) struct Admission {
    /// Budget for analytics queries.
    analytics: Arc<Semaphore>,
    /// Budget for log queries.
    logs: Arc<Semaphore>,
    /// Budget for interactive commands.
    exec: Arc<Semaphore>,
    /// Budget for open event streams.
    event_streams: Arc<Semaphore>,
    /// Budget for open log streams.
    log_streams: Arc<Semaphore>,
    /// Budget for installation checks.
    checks: Arc<Semaphore>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            analytics: Expensive::Analytics.budget(),
            logs: Expensive::Logs.budget(),
            exec: Expensive::Exec.budget(),
            event_streams: Expensive::EventStream.budget(),
            log_streams: Expensive::LogStream.budget(),
            checks: Expensive::Checks.budget(),
        }
    }
}

impl Admission {
    /// The budget of `kind`.
    fn of(&self, kind: Expensive) -> &Arc<Semaphore> {
        match kind {
            Expensive::Analytics => &self.analytics,
            Expensive::Logs => &self.logs,
            Expensive::Exec => &self.exec,
            Expensive::EventStream => &self.event_streams,
            Expensive::LogStream => &self.log_streams,
            Expensive::Checks => &self.checks,
        }
    }
}

impl ApplicationService {
    /// Admits one `kind` of expensive work, for as long as the returned
    /// permit is held.
    ///
    /// # Errors
    /// Returns [`ApplicationError::Busy`] when as many already run as `kind`
    /// allows.
    #[expect(
        clippy::map_err_ignore,
        reason = "the only failure is no free permit, which `Busy` names"
    )]
    pub fn admit(&self, kind: Expensive) -> Result<OwnedSemaphorePermit, ApplicationError> {
        Arc::clone(self.admission.of(kind))
            .try_acquire_owned()
            .map_err(|_| ApplicationError::Busy(kind))
    }
}
