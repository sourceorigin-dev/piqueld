//! The build queue: a set number of image builds at a time, environments
//! before previews.
//!
//! Builds wait in two queues, each first come, first served, while every
//! slot is taken. When a build finishes, its slot goes to the oldest waiting
//! environment build, unless
//! [`ENVIRONMENT_BURST`] environment builds already went ahead of waiting
//! preview builds: then the oldest preview build goes. So environments go
//! first, but a steady stream of their builds never starves previews. A
//! running build is never interrupted, and a cancelled one leaves its queue.
use parking_lot::Mutex;
use piqueld_core::EnvironmentKind;
use std::{collections::VecDeque, num::NonZeroU8};
use tokio::sync::oneshot;

/// Environment builds that may go ahead of waiting preview builds before the
/// oldest preview build goes next.
const ENVIRONMENT_BURST: u32 = 3;

/// The queue a build waits in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildPriority {
    /// An environment's build, which goes first.
    #[default]
    Environment,
    /// A preview's build.
    Preview,
}

impl From<&EnvironmentKind> for BuildPriority {
    fn from(kind: &EnvironmentKind) -> Self {
        match kind {
            EnvironmentKind::Environment => Self::Environment,
            EnvironmentKind::Preview(_) => Self::Preview,
        }
    }
}

/// Gives a set number of builds at a time their turn; see the module
/// documentation.
pub(crate) struct BuildQueue {
    /// How many builds may run at once.
    slots: NonZeroU8,
    /// The builds waiting and running.
    state: Mutex<Waiting>,
}

/// The builds waiting for their turn, each woken through its sender.
#[derive(Default)]
struct Waiting {
    /// How many builds have a turn. It only drops below the queue's slots
    /// while no build waits.
    running: u8,
    /// The waiting environment builds, in order.
    environments: VecDeque<oneshot::Sender<()>>,
    /// The waiting preview builds, in order.
    previews: VecDeque<oneshot::Sender<()>>,
    /// Environment builds that went ahead of waiting preview builds since a
    /// preview build last had the turn.
    overtaken: u32,
}

impl BuildQueue {
    /// Creates a queue that runs `slots` builds at a time.
    pub(crate) fn new(slots: NonZeroU8) -> Self {
        Self {
            slots,
            state: Mutex::default(),
        }
    }

    /// Waits for a build's turn, which lasts until the returned [`Turn`] is
    /// dropped. Cancelling the wait leaves the queue.
    ///
    /// # Panics
    /// Panics if the queue drops a waiting build, which it never does.
    #[expect(
        clippy::expect_used,
        reason = "The queue wakes every build it keeps before dropping its sender."
    )]
    pub(crate) async fn turn(&self, priority: BuildPriority) -> Turn<'_> {
        let receiver = {
            let mut waiting = self.state.lock();
            if waiting.running < self.slots.get() {
                waiting.running += 1;
                return Turn(self);
            }
            let (sender, receiver) = oneshot::channel();
            match priority {
                BuildPriority::Environment => waiting.environments.push_back(sender),
                BuildPriority::Preview => waiting.previews.push_back(sender),
            }
            receiver
        };
        let mut wait = Wait {
            queue: self,
            receiver,
            woken: false,
        };
        (&mut wait.receiver)
            .await
            .expect("the queue wakes every build it keeps");
        wait.woken = true;
        Turn(self)
    }

    /// Hands a finished build's slot to the next waiting build, or frees it.
    fn pass(&self) {
        let mut waiting = self.state.lock();
        while let Some(next) = waiting.next() {
            if next.send(()).is_ok() {
                return;
            }
        }
        waiting.running -= 1;
    }
}

impl Waiting {
    /// Removes the next build to wake, skipping cancelled ones.
    fn next(&mut self) -> Option<oneshot::Sender<()>> {
        self.environments.retain(|build| !build.is_closed());
        self.previews.retain(|build| !build.is_closed());
        if (self.previews.is_empty() || self.overtaken < ENVIRONMENT_BURST)
            && let Some(next) = self.environments.pop_front()
        {
            if !self.previews.is_empty() {
                self.overtaken += 1;
            }
            return Some(next);
        }
        self.overtaken = 0;
        self.previews.pop_front()
    }
}

/// A running build's turn; dropping it passes the turn on.
pub(crate) struct Turn<'a>(&'a BuildQueue);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.pass();
    }
}

/// A build waiting for its turn. Dropped while waiting, it leaves the queue
/// and passes on a turn it was handed meanwhile.
struct Wait<'a> {
    /// The queue it waits in.
    queue: &'a BuildQueue,
    /// Resolves when the build's turn comes.
    receiver: oneshot::Receiver<()>,
    /// Whether the build took its turn.
    woken: bool,
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        if !self.woken {
            self.receiver.close();
            if self.receiver.try_recv().is_ok() {
                self.queue.pass();
            }
        }
    }
}

#[cfg(test)]
mod tests;
