//! Logic of the dashboard shell and its read-only views, kept apart from
//! their rendering in `browser/` so it compiles and is tested natively:
//! application health summaries and recent deployments, preview limits,
//! notification destinations, and build history.

mod builds;
mod dashboard;
#[cfg(test)]
mod fixtures;
mod notifications;

pub use builds::{BuildList, OutputChunks, build_arguments};
pub use dashboard::{ApplicationRow, EnvironmentRow, PreviewLoad};
pub use notifications::{DestinationDraft, Receives};
