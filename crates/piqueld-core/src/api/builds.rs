//! Source builds and job runs, and their output.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::LogStream;
use crate::names::string_enum;

/// Durable source-preparation attempt or one-shot job run, independent of the executor.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct BuildRecord {
    /// Monotonic build identifier.
    pub id: i64,
    /// Owning environment ID.
    pub environment_id: crate::EnvironmentId,
    /// Source operation ID, retained even after operation pruning.
    pub operation_id: String,
    /// Logical service name; for job runs, the service whose container the job reuses.
    pub service: String,
    /// Logical job name when this record is a job run rather than a source build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Requested build source.
    pub source: crate::manifest::ValidatedSource,
    /// Current attempt outcome.
    pub state: BuildState,
    /// Start time in Unix milliseconds.
    pub started_at_ms: i64,
    /// Completion time, absent while running.
    pub finished_at_ms: Option<i64>,
    /// Resolved Git commit, when checkout completed.
    pub commit: Option<String>,
    /// Built image identifier, when successful; for job runs, the image that ran.
    pub image_id: Option<String>,
    /// Where the image was built, once it was: absent for job runs and
    /// builds that reused an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_on: Option<BuiltOn>,
    /// The earlier build whose image this build reused instead of building,
    /// because its commit and build settings were the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_build: Option<i64>,
    /// Job container exit code, when the job ran to completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Number of currently retained output bytes.
    pub log_bytes: i64,
    /// Output exceeded the configured per-build cap.
    pub log_truncated: bool,
    /// Output was removed by retention.
    pub log_expired: bool,
}

/// Where an image was built.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuiltOn {
    /// The host's Docker Engine.
    Host,
    /// The host's build machine, which pushed it to the build registry.
    BuildMachine,
}

impl BuiltOn {
    /// Where `image`, a built image's local ID or repository digest, was
    /// built.
    #[must_use]
    pub fn of(image: &crate::BuiltImage) -> Self {
        match image {
            crate::BuiltImage::Local(_) => Self::Host,
            crate::BuiltImage::Pushed(_) => Self::BuildMachine,
        }
    }
}

string_enum! {
    #[serde]
    /// Outcome of a source-preparation attempt or job run.
    pub enum BuildState {
        /// Source preparation is running.
        Running => "running",
        /// An image was produced successfully, or the job exited with status zero.
        Succeeded => "succeeded",
        /// Checkout or build execution failed, or the job failed or timed out.
        Failed => "failed",
        /// Execution was cancelled or interrupted by daemon shutdown.
        Interrupted => "interrupted",
    }
}

/// Bounded byte-offset page of build output.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct BuildLogPage {
    /// Structured output chunks in chronological order.
    pub items: Vec<BuildLogChunk>,
    /// Exclusive byte cursor for loading an older page.
    pub previous_offset: Option<i64>,
    /// The build exceeded its total output cap.
    pub truncated: bool,
    /// Retention removed the output.
    pub expired: bool,
}

/// Captured output with stream identity and capture time.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct BuildLogChunk {
    /// Byte position in the retained output.
    pub offset: i64,
    /// Capture time in Unix milliseconds.
    pub timestamp_ms: i64,
    /// Captured source stream.
    pub stream: LogStream,
    /// Lossy UTF-8 output, possibly containing partial lines.
    pub text: String,
}

impl BuildLogChunk {
    /// How much of `bytes` a chunk takes now, keeping an incomplete UTF-8
    /// suffix for the next read without rejecting binary output.
    ///
    /// Returns how many leading bytes can be flushed now: everything except a
    /// trailing, possibly incomplete multi-byte character. Invalid bytes elsewhere
    /// are flushed as-is.
    ///
    /// ```text
    /// b"ok\xe2\x82"      -> 2  (the partial `€` waits for more bytes)
    /// b"\xffok\xe2\x82"  -> 3
    /// ```
    #[must_use]
    pub fn complete_prefix(bytes: &[u8]) -> usize {
        let mut end = 0;
        for chunk in bytes.utf8_chunks() {
            end += chunk.valid().len();
            let invalid = chunk.invalid();
            if std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none()) {
                return end;
            }
            end += invalid.len();
        }
        end
    }
}
