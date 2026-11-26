//! Live container output of an environment's services, followed as Docker
//! writes it, picking up the tasks a rollout starts.
use super::{ApplicationError, ApplicationService};
use crate::application::{BoundaryError, RuntimeBoundary};
use futures_util::{StreamExt as _, stream::BoxStream, stream::SelectAll};
use piqueld_core::{
    EnvironmentId,
    api::{LogRecord, LogSource, LogStream, LogTime},
};
use piqueld_docker::{ContainerStarts, ContainerState, DockerError, LogLines, Workload, now_ns};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

/// The furthest back a live log stream starts, in nanoseconds: a day, the
/// longest window a log snapshot reads.
const MAX_REPLAY_NS: i64 = 86_400 * 1_000_000_000;

/// The output of one environment's services, as it is written: each of
/// their containers is followed until it stops, and containers that start
/// later, such as a rollout's new tasks, once Docker reports them starting.
pub struct LogFollower {
    /// Runtime the logs are read from.
    runtime: Arc<dyn RuntimeBoundary>,
    /// Environment whose logs are followed.
    environment: EnvironmentId,
    /// The only service followed, or all of them.
    service: Option<String>,
    /// The only stream followed, or both.
    stream: Option<LogStream>,
    /// Output written at or before this, in Unix nanoseconds, is skipped.
    after: i64,
    /// Containers followed, by ID.
    followed: HashSet<String>,
    /// Containers listed once started, and those whose start already made it
    /// list them again, by ID: their starts list nothing again, so another
    /// environment's containers cost one listing each at most.
    seen: HashSet<String>,
    /// Each followed container's records, as Docker sends them.
    followers: SelectAll<BoxStream<'static, Vec<LogRecord>>>,
    /// Container starts since `after`, which Docker replays.
    starts: ContainerStarts,
    /// Whether a container this follower doesn't know started since the
    /// containers were last listed.
    rescan: bool,
    /// Records read, not yet returned.
    pending: VecDeque<LogRecord>,
}

impl ApplicationService {
    /// Follows environment `id`'s output written after `after`, in Unix
    /// nanoseconds, or from now; at most a day back, and never later than now,
    /// so a client clock running ahead skips nothing. `service` and `stream`
    /// narrow it as they do a snapshot. Starts are watched before the
    /// containers are listed, so none starts unseen.
    /// # Errors
    /// Returns an invalid query, a missing environment, or runtime errors.
    pub async fn follow_logs(
        &self,
        id: &EnvironmentId,
        service: Option<&str>,
        stream: Option<LogStream>,
        after: Option<i64>,
    ) -> Result<LogFollower, ApplicationError> {
        Self::check_log_service(service)?;
        self.store.get(id).await?;
        let now = now_ns();
        let after = after.unwrap_or(now).clamp(now - MAX_REPLAY_NS, now);
        let mut follower = LogFollower {
            starts: self
                .runtime
                .container_starts(after.div_euclid(1_000_000_000)),
            runtime: Arc::clone(&self.runtime),
            environment: id.clone(),
            service: service.map(str::to_owned),
            stream,
            after,
            followed: HashSet::new(),
            seen: HashSet::new(),
            followers: SelectAll::new(),
            rescan: true,
            pending: VecDeque::new(),
        };
        follower.discover().await?;
        Ok(follower)
    }
}

impl LogFollower {
    /// The next record written. The output of each container comes in
    /// order; containers' output interleaves as it arrives, so records of
    /// different containers may come out of timestamp order. Cancel-safe.
    /// # Errors
    /// Returns runtime errors once containers can no longer be listed or
    /// their starts watched; the follower should then be dropped.
    pub async fn next(&mut self) -> Result<LogRecord, BoundaryError> {
        loop {
            if let Some(record) = self.pending.pop_front() {
                return Ok(record);
            }
            if self.rescan {
                self.discover().await?;
                continue;
            }
            tokio::select! {
                Some(records) = self.followers.next(), if !self.followers.is_empty() => {
                    self.pending.extend(records);
                }
                start = self.starts.next() => match start {
                    Some(Ok(Some(id))) => self.rescan |= self.seen.insert(id),
                    Some(Ok(None)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => {
                        return Err(DockerError::Request("watch container starts").into());
                    }
                },
            }
        }
    }

    /// Lists the containers and follows those of the environment's services
    /// it doesn't follow yet. A container still being created is skipped,
    /// as its start brings it back; one that vanishes, or can no longer be
    /// read, ends its output.
    async fn discover(&mut self) -> Result<(), BoundaryError> {
        for container in self.runtime.containers().await? {
            if container.state == ContainerState::Created {
                continue;
            }
            self.seen.insert(container.id.clone());
            let Workload::Service { environment, name } = container.workload else {
                continue;
            };
            if environment != self.environment
                || self
                    .service
                    .as_ref()
                    .is_some_and(|service| *service != name)
                || !self.followed.insert(container.id.clone())
            {
                continue;
            }
            let (after, stream) = (self.after, self.stream);
            let task = container.task.unwrap_or_default();
            let since = i32::try_from(after.div_euclid(1_000_000_000)).unwrap_or(i32::MAX);
            self.followers.push(
                self.runtime
                    .container_logs(&container.id, since)
                    .take_while(|lines| std::future::ready(lines.is_ok()))
                    .filter_map(|lines| std::future::ready(lines.ok()))
                    .map(move |lines| Self::records(lines, &name, &task, after, stream))
                    .boxed(),
            );
        }
        self.rescan = false;
        Ok(())
    }

    /// The records of `lines`, written by `task` of `service`, that come
    /// after `after` and are on `stream` when one is chosen.
    fn records(
        lines: LogLines,
        service: &str,
        task: &str,
        after: i64,
        stream: Option<LogStream>,
    ) -> Vec<LogRecord> {
        let Some(written) = lines
            .stream
            .filter(|written| stream.is_none_or(|s| s == *written))
        else {
            return Vec::new();
        };
        lines
            .lines
            .into_iter()
            .filter(|(timestamp, _)| *timestamp > after)
            .map(|(timestamp, message)| LogRecord {
                service: service.to_owned(),
                task_id: task.to_owned(),
                timestamp: LogTime(timestamp).to_string(),
                stream: LogSource::from(written).into(),
                message: LogRecord::clean_message(&message),
            })
            .collect()
    }
}
