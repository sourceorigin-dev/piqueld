//! Shared process execution for Git and Docker, retaining only diagnostic tails.
use anyhow::Context;
use std::{collections::VecDeque, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Each command owns a process group, so cancellation also stops helpers such
/// as Git transports and Docker build plugins before the caller releases its slot.
/// Dropping the guard kills the group unless it was disarmed by clearing the PID.
struct ProcessGroup(Option<rustix::process::Pid>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(id) = self.0 else {
            return;
        };
        if let Err(error) = rustix::process::kill_process_group(id, rustix::process::Signal::KILL)
            && error != rustix::io::Errno::SRCH
        {
            tracing::warn!(?error, "failed to stop command process group");
        }
    }
}

/// Where a command streams its output as it runs, such as a build's
/// durable log.
#[async_trait::async_trait]
pub trait OutputLog: Send + Sync {
    /// Appends `bytes` the command wrote to `stream`. They end on a character
    /// boundary unless the command wrote invalid UTF-8.
    ///
    /// # Errors
    /// The output couldn't be recorded; the command then fails rather than
    /// silently losing it.
    async fn append(
        &self,
        bytes: &[u8],
        stream: piqueld_core::api::LogStream,
    ) -> anyhow::Result<()>;
}

/// Namespace for running external commands with bounded output capture.
pub struct LoggedCommand;

/// Typed facts are safe to expose; output tails remain internal diagnostics.
#[derive(Debug, thiserror::Error)]
#[error("{operation} failed ({status}):\nstdout: {stdout}\nstderr: {stderr}")]
pub struct CommandFailure {
    /// What the command was for, such as `clone Git repository`.
    pub operation: &'static str,
    /// How the command exited.
    pub status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// A command printed more than [`LoggedCommand::output`] reads.
#[derive(Debug, thiserror::Error)]
#[error("output exceeds {0} bytes")]
pub struct OutputLimit(pub usize);

impl LoggedCommand {
    /// Bytes of each output stream retained for failure diagnostics.
    const TAIL_BYTES: usize = 8192;

    /// Drain both streams concurrently with a fixed memory bound, streaming them
    /// into `log` when supplied. A non-zero exit returns a [`CommandFailure`] with
    /// the last `TAIL_BYTES` of each stream. The command runs in its own process
    /// group, which is killed if this future is dropped before the command exits.
    /// Error tails stay in internal diagnostics, never the public API response.
    ///
    /// # Errors
    /// The command can't start, exits unsuccessfully ([`CommandFailure`]), or
    /// its output can't be read or recorded.
    pub async fn run_recorded(
        command: &mut Command,
        operation: &'static str,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> anyhow::Result<()> {
        let stdout =
            |stdout| Self::tail_recorded(stdout, log, piqueld_core::api::LogStream::Stdout);
        Self::run_in_group(command, operation, stdout, log)
            .await
            .map(drop)
    }

    /// Runs a command like [`Self::run_recorded`], without a log, and returns
    /// its whole standard output. More than `limit` bytes is an
    /// [`OutputLimit`] error.
    ///
    /// # Errors
    /// The errors of [`Self::run_recorded`], and [`OutputLimit`].
    pub async fn output(
        command: &mut Command,
        operation: &'static str,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let stdout = async |stdout: tokio::process::ChildStdout| {
            let mut output = Vec::new();
            stdout
                .take(u64::try_from(limit)?.saturating_add(1))
                .read_to_end(&mut output)
                .await?;
            if output.len() > limit {
                return Err(OutputLimit(limit).into());
            }
            Ok(output)
        };
        Self::run_in_group(command, operation, stdout, None).await
    }

    /// Runs a command in its own process group, reading its standard output
    /// with `read_stdout` and the last `TAIL_BYTES` of its standard error,
    /// streamed into `log` when supplied. Returns what `read_stdout` read.
    async fn run_in_group<F: Future<Output = anyhow::Result<Vec<u8>>>>(
        command: &mut Command,
        operation: &'static str,
        read_stdout: impl FnOnce(tokio::process::ChildStdout) -> F,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .with_context(|| operation)?;
        let mut group = ProcessGroup(Some(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw)
                .context("capture command process group")?,
        ));
        let stdout = child.stdout.take().context("capture command stdout")?;
        let stderr = child.stderr.take().context("capture command stderr")?;
        // Keep the leader unreaped while draining pipes. Its reserved PID prevents
        // the process-group ID from being reused while cancellation can signal it.
        let (stdout, stderr) = tokio::try_join!(
            read_stdout(stdout),
            Self::tail_recorded(stderr, log, piqueld_core::api::LogStream::Stderr),
        )
        .with_context(|| operation)?;
        let status = child.wait().await.with_context(|| operation)?;
        // No await may intervene between reaping the leader and disarming.
        group.0 = None;
        if !status.success() {
            return Err(CommandFailure {
                operation,
                status,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            }
            .into());
        }
        Ok(stdout)
    }

    /// Reads a stream to the end, returning its last `TAIL_BYTES`.
    ///
    /// When `log` is supplied, output is appended in [`Batch`]es as it
    /// arrives, holding back an incomplete trailing UTF-8 character until the
    /// next read or end of stream.
    async fn tail_recorded(
        mut stream: impl AsyncRead + Unpin,
        log: Option<&dyn crate::command::OutputLog>,
        source: piqueld_core::api::LogStream,
    ) -> anyhow::Result<Vec<u8>> {
        let mut tail = VecDeque::with_capacity(Self::TAIL_BYTES);
        let mut buffer = [0; 4096];
        let mut batch = log.map(|log| Batch::new(log, source));
        loop {
            let due = batch.as_ref().and_then(|batch| batch.due);
            let read = match due {
                // Reading is cancel safe: a read that times out read nothing.
                Some(due) => match tokio::time::timeout_at(due, stream.read(&mut buffer)).await {
                    Ok(read) => read?,
                    Err(_elapsed) => {
                        if let Some(batch) = &mut batch {
                            batch.append(false).await?;
                        }
                        continue;
                    }
                },
                None => stream.read(&mut buffer).await?,
            };
            if read == 0 {
                if let Some(batch) = &mut batch {
                    batch.append(true).await?;
                }
                return Ok(tail.into());
            }
            if let Some(batch) = &mut batch {
                batch.add(&buffer[..read]).await?;
            }
            let discard = (tail.len() + read).saturating_sub(Self::TAIL_BYTES);
            tail.drain(..discard);
            tail.extend(&buffer[..read]);
        }
    }
}

/// Output of one stream read but not yet appended to its log. Appending in
/// batches takes a durable log's writer once per batch instead of once per
/// read, while output still shows within [`Batch::DELAY`].
struct Batch<'a> {
    log: &'a dyn OutputLog,
    stream: piqueld_core::api::LogStream,
    pending: Vec<u8>,
    /// When pending output is appended even if no more arrives.
    due: Option<tokio::time::Instant>,
}

impl<'a> Batch<'a> {
    /// Pending bytes that are appended at once.
    const BYTES: usize = 64 * 1024;
    /// How long output may wait for more before it is appended.
    const DELAY: std::time::Duration = std::time::Duration::from_millis(500);

    fn new(log: &'a dyn OutputLog, stream: piqueld_core::api::LogStream) -> Self {
        Self {
            log,
            stream,
            pending: Vec::new(),
            due: None,
        }
    }

    /// Adds output read from the stream, appending the batch once it holds
    /// [`Self::BYTES`].
    async fn add(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= Self::BYTES {
            return self.append(false).await;
        }
        self.due
            .get_or_insert_with(|| tokio::time::Instant::now() + Self::DELAY);
        Ok(())
    }

    /// Appends the pending output. Unless the stream `ended`, an incomplete
    /// trailing character waits for the rest of its bytes.
    async fn append(&mut self, ended: bool) -> anyhow::Result<()> {
        self.due = None;
        let end = if ended {
            self.pending.len()
        } else {
            piqueld_core::api::BuildLogChunk::complete_prefix(&self.pending)
        };
        if end > 0 {
            self.log.append(&self.pending[..end], self.stream).await?;
            self.pending.drain(..end);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
