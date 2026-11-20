//! Durable attempt lifecycle and output sink, independent of the executor.
use piqueld_core::{
    EnvironmentId,
    api::{BuildState, LogStream},
    manifest::ValidatedSource as Source,
};
use piqueld_store::{BuildOutcome, LogAppend, LogEnding, Store, StoreError};
use std::{collections::VecDeque, sync::Arc};

/// A bounded persistent output sink usable by any build executor.
///
/// Output fills the first half of the build's byte cap as it arrives. Past
/// that, only the newest output is kept, in memory, until [`BuildLog::finish`]
/// writes it after a note of how many bytes it omitted: a long log keeps its
/// start and its failure, and stops taking the store's writer once its first
/// half is full.
#[derive(Clone)]
pub struct BuildLog {
    store: Arc<Store>,
    id: i64,
    // Held across store writes, so output is stored in the order it arrived.
    output: Arc<tokio::sync::Mutex<Kept>>,
}
impl BuildLog {
    fn new(store: Arc<Store>, id: i64) -> Self {
        let cap = usize::try_from(store.build_log_max_bytes()).unwrap_or(usize::MAX);
        Self {
            store,
            id,
            output: Arc::new(tokio::sync::Mutex::new(Kept::new(cap))),
        }
    }
    /// Appends output, keeping the start and the end of it within the
    /// configured cap.
    /// # Errors
    /// Returns a persistence error rather than silently losing output.
    pub async fn append(&self, bytes: &[u8], stream: LogStream) -> Result<(), StoreError> {
        let mut output = self.output.lock().await;
        if output.closed || output.dropped {
            return Ok(());
        }
        let head = output.head(bytes);
        if !head.is_empty()
            && self.store.append_build_log(self.id, head, stream).await? == LogAppend::Closed
        {
            output.closed = true;
            return Ok(());
        }
        output.keep(&bytes[head.len()..], stream);
        Ok(())
    }
    /// Finishes the build with `outcome` and, in the same transaction, writes
    /// the output kept past the first half of the cap, after a note of how
    /// many bytes were omitted, and marks the log truncated if any output was
    /// dropped. The kept output is let go only once that commits, so a failed
    /// or cancelled finish can be retried. Later output is not accepted.
    async fn finish(&self, outcome: &BuildOutcome) -> Result<(), StoreError> {
        let mut output = self.output.lock().await;
        self.store
            .finish_build(self.id, outcome, &output.ending())
            .await?;
        output.finished();
        Ok(())
    }
    /// Records the resolved Git commit on the build.
    pub(crate) async fn commit(&self, commit: &str) -> Result<(), StoreError> {
        self.store.build_commit(self.id, commit).await
    }
    /// Records a finished job run's exit code.
    pub(crate) async fn exit_code(&self, code: i64) -> Result<(), StoreError> {
        self.store.build_exit_code(self.id, code).await
    }
    /// Records that output was dropped before reaching this log, which marks
    /// it truncated as the build finishes. Later output is not accepted.
    pub(crate) async fn truncated(&self) {
        self.output.lock().await.dropped = true;
    }
}

/// What a [`BuildLog`] keeps of output that is not stored yet.
struct Kept {
    /// Bytes still written as they arrive.
    head: usize,
    /// The newest output past the head, oldest first, merged per stream.
    tail: VecDeque<(LogStream, Vec<u8>)>,
    tail_bytes: usize,
    /// The most bytes the tail keeps.
    tail_max: usize,
    /// The most bytes the omission note may take; it is left out when longer.
    note_max: usize,
    /// Bytes dropped between the head and the tail.
    omitted: usize,
    /// Output was dropped before reaching the log, which accepts no more.
    dropped: bool,
    /// The log takes no more output: the store refused it, or the build
    /// finished.
    closed: bool,
}
impl Kept {
    /// Bytes of a build's cap set aside for the omission note, which fit any
    /// count of omitted bytes.
    const NOTE_BYTES: usize = 64;

    /// Splits a build's byte cap: half for the head, then the omission note,
    /// then the tail. A small cap keeps at least half of its second half for
    /// the tail.
    fn new(cap: usize) -> Self {
        let head = cap / 2;
        let rest = cap - head;
        let note_max = Self::NOTE_BYTES.min(rest / 2);
        Self {
            head,
            tail: VecDeque::new(),
            tail_bytes: 0,
            tail_max: rest - note_max,
            note_max,
            omitted: 0,
            dropped: false,
            closed: false,
        }
    }
    /// The start of `bytes` that still fits in the head, cut on a character
    /// boundary. Once some output doesn't fit, the head takes no more.
    fn head<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        if bytes.len() <= self.head {
            self.head -= bytes.len();
            return bytes;
        }
        let end = piqueld_core::api::BuildLogChunk::complete_prefix(&bytes[..self.head]);
        self.head = 0;
        &bytes[..end]
    }
    /// Keeps `bytes` in the tail, dropping its oldest output past
    /// `tail_max`, cut so the tail starts on a character boundary.
    fn keep(&mut self, bytes: &[u8], stream: LogStream) {
        if bytes.is_empty() {
            return;
        }
        match self.tail.back_mut() {
            Some((last, kept)) if *last == stream => kept.extend_from_slice(bytes),
            _ => self.tail.push_back((stream, bytes.to_vec())),
        }
        self.tail_bytes += bytes.len();
        while self.tail_bytes > self.tail_max
            && let Some((_, oldest)) = self.tail.front_mut()
        {
            let excess = self.tail_bytes - self.tail_max;
            let cut = oldest
                .iter()
                .skip(excess)
                .position(|byte| byte & 0xC0 != 0x80)
                .map_or(oldest.len(), |start| excess + start);
            if cut == oldest.len() {
                self.tail.pop_front();
            } else {
                oldest.drain(..cut);
            }
            self.tail_bytes -= cut;
            self.omitted += cut;
        }
    }
    /// What finishing the build writes: the tail, preceded by a note when
    /// output was omitted and the note fits, and whether any output was
    /// dropped. Nothing once the log is closed.
    fn ending(&self) -> LogEnding {
        if self.closed {
            return LogEnding::default();
        }
        let note = (self.omitted > 0)
            .then(|| format!("\npiqueld: … {} bytes omitted …\n", self.omitted))
            .filter(|note| note.len() <= self.note_max)
            .map(|note| (LogStream::Stderr, note.into_bytes()));
        LogEnding {
            output: note.into_iter().chain(self.tail.iter().cloned()).collect(),
            truncated: self.omitted > 0 || self.dropped,
        }
    }
    /// Lets go of the tail once the build finished: the log takes no more
    /// output.
    fn finished(&mut self) {
        self.tail.clear();
        self.tail_bytes = 0;
        self.omitted = 0;
        self.closed = true;
    }
}

#[async_trait::async_trait]
impl piqueld_docker::command::OutputLog for BuildLog {
    async fn append(&self, bytes: &[u8], stream: LogStream) -> anyhow::Result<()> {
        Ok(BuildLog::append(self, bytes, stream).await?)
    }
}

/// A persisted build record that is always given a terminal state.
///
/// Call [`BuildAttempt::finish`] on completion. If the attempt is dropped first
/// (e.g. the preparation future is cancelled) or finishing fails, `Drop` records
/// the outcome in the background.
pub(crate) struct BuildAttempt {
    pub(crate) log: BuildLog,
    completion: Completion,
}

/// How far a build's terminal state has been persisted.
enum Completion {
    /// The build has not finished; dropping it records `Interrupted`.
    Running,
    /// A terminal state was chosen but its write has not succeeded yet.
    Pending(BuildOutcome),
    /// The terminal state is stored; nothing is left to record.
    Persisted,
}
impl Completion {
    /// Returns the outcome `Drop` still needs to persist, if any.
    fn retry(self) -> Option<BuildOutcome> {
        match self {
            Self::Running => Some(BuildOutcome::without_image(BuildState::Interrupted)),
            Self::Pending(outcome) => Some(outcome),
            Self::Persisted => None,
        }
    }
}
impl BuildAttempt {
    /// Creates a running build record for one service of an operation, or a
    /// job run record when `job` is set.
    pub(crate) async fn start(
        store: Arc<Store>,
        application: &EnvironmentId,
        operation: &str,
        service: &str,
        source: &Source,
        job: Option<&str>,
    ) -> Result<Self, StoreError> {
        let id = store
            .start_build(application, operation, service, source, job)
            .await?;
        Ok(Self {
            log: BuildLog::new(store, id),
            completion: Completion::Running,
        })
    }
    /// Persists the terminal state and, on success, the built image ID.
    pub(crate) async fn finish(
        self,
        state: BuildState,
        image: Option<&str>,
    ) -> Result<(), StoreError> {
        self.complete(BuildOutcome {
            state,
            image: image.map(str::to_owned),
            reused_build: None,
        })
        .await
    }
    /// Finishes the build as succeeded with the image `earlier` produced,
    /// and says so in its output: nothing was built.
    pub(crate) async fn reuse(
        self,
        earlier: &piqueld_store::ReusableBuild,
    ) -> Result<(), StoreError> {
        self.log
            .append(
                format!(
                    "Reusing the image of build #{}: the commit and build settings are the same. \
                     Deploy with a rebuild to build it again.\n",
                    earlier.id
                )
                .as_bytes(),
                LogStream::Stdout,
            )
            .await?;
        self.complete(BuildOutcome {
            state: BuildState::Succeeded,
            image: Some(earlier.image.to_string()),
            reused_build: Some(earlier.id),
        })
        .await
    }
    /// Persists `outcome` with the rest of the output, which `Drop` retries
    /// if the write fails or is cancelled.
    async fn complete(mut self, outcome: BuildOutcome) -> Result<(), StoreError> {
        self.completion = Completion::Pending(outcome.clone());
        self.log.finish(&outcome).await?;
        self.completion = Completion::Persisted;
        Ok(())
    }
}
impl Drop for BuildAttempt {
    /// Spawns a best-effort write of any unpersisted terminal state, with the
    /// rest of the output. If that also fails, startup recovery marks the
    /// build interrupted.
    fn drop(&mut self) {
        let Some(outcome) = std::mem::replace(&mut self.completion, Completion::Persisted).retry()
        else {
            return;
        };
        let log = self.log.clone();
        tokio::spawn(async move {
            if let Err(error) = log.finish(&outcome).await {
                tracing::error!(build_id=log.id,error=?error,"failed to record build completion; startup recovery will mark it interrupted");
            }
        });
    }
}

#[cfg(test)]
mod tests;
