//! Read owned container output on demand, without persisting it. The
//! daemon follows live output through `DockerApi::container_logs` instead.
use super::{BollardDocker, BollardResult, DockerError};
use bollard::{
    container::LogOutput,
    models::Task,
    query_parameters::{ListServicesOptionsBuilder, ListTasksOptionsBuilder, LogsOptionsBuilder},
};
use futures_util::StreamExt;
use piqueld_core::{
    EnvironmentId, InstanceId,
    api::{ApplicationLogs, LogRecord},
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, SERVICE_LABEL},
};
use std::{
    cmp::Reverse,
    collections::{HashMap, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};

/// Most tasks whose containers one log read reads.
const MAX_LOG_TASKS: usize = 256;
/// Accounted output bytes one log read shares between its tasks.
const LOG_BYTES: usize = 1024 * 1024;
/// The least of [`LOG_BYTES`] each task gets, however many share it.
const TASK_LOG_BYTES: usize = 64 * 1024;
/// Containers whose logs are read at once.
const LOG_READS: usize = 8;

/// What a log read asks of each container.
#[derive(Clone, Copy)]
struct LogWindow {
    /// Lines to keep in the end, of all containers together.
    tail: u16,
    /// Docker's absolute `since` Unix timestamp.
    since: i32,
    /// The only stream to read, or both.
    stream: Option<piqueld_core::api::LogStream>,
    /// Accounted bytes each task keeps of its newest output.
    budget: usize,
}

/// The newest output of one task's container, within its byte budget.
struct TaskLog {
    /// The retained records with the bytes each accounts for, oldest first.
    records: VecDeque<(LogRecord, usize)>,
    /// The bytes `records` account for.
    bytes: usize,
    /// The most bytes `records` may account for.
    budget: usize,
    /// Whether output was dropped: past the budget, or a container vanished.
    truncated: bool,
}

impl TaskLog {
    /// An empty log that keeps up to `budget` bytes.
    fn new(budget: usize) -> Self {
        Self {
            records: VecDeque::new(),
            bytes: 0,
            budget,
            truncated: false,
        }
    }

    /// Adds `record`, which accounts for `size` bytes, dropping the oldest
    /// records past the budget.
    fn push(&mut self, record: LogRecord, size: usize) {
        self.bytes += size;
        self.records.push_back((record, size));
        while self.bytes > self.budget
            && let Some((_, dropped)) = self.records.pop_front()
        {
            self.bytes -= dropped;
            self.truncated = true;
        }
    }
}

impl BollardDocker {
    /// Reads a bounded, merged log window for one application's services.
    ///
    /// Lists the services' tasks, running ones first, then the most recently
    /// updated, and keeps the first [`MAX_LOG_TASKS`]. Reads their containers'
    /// timestamped output [`LOG_READS`] at a time: at most `tail + 1` lines
    /// since `since` seconds ago, each task keeping its newest output within
    /// an equal share of [`LOG_BYTES`], at least [`TASK_LOG_BYTES`]. Returns
    /// the newest `tail` records by timestamp then task ID. `truncated` is set
    /// whenever tasks or records were skipped, or a limit was hit.
    pub(super) async fn read_logs(
        &self,
        instance: &InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<ApplicationLogs, DockerError> {
        let services = self.owned_services(instance, application, service).await?;
        if services.is_empty() {
            return Ok(ApplicationLogs::default());
        }
        let mut tasks = self
            .client()?
            .list_tasks(Some(
                ListTasksOptionsBuilder::default()
                    .filters(&HashMap::from([(
                        "service",
                        services.keys().cloned().collect::<Vec<_>>(),
                    )]))
                    .build(),
            ))
            .await
            .or_request("list log tasks")?;
        let mut result = ApplicationLogs {
            truncated: tasks.len() > MAX_LOG_TASKS,
            ..ApplicationLogs::default()
        };
        Self::log_order(&mut tasks);
        let window = LogWindow {
            tail,
            since: Self::log_since(since),
            stream,
            budget: (LOG_BYTES / tasks.len().max(1)).max(TASK_LOG_BYTES),
        };
        let reads = tasks
            .iter()
            .map(|task| self.task_logs(task, &services, window))
            .collect::<Vec<_>>();
        let mut reads = futures_util::stream::iter(reads).buffer_unordered(LOG_READS);
        while let Some(read) = reads.next().await {
            let read = read?;
            result.truncated |= read.truncated;
            result
                .items
                .extend(read.records.into_iter().map(|(record, _)| record));
        }
        result.truncated |= result.items.len() > usize::from(tail);
        result.items.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then(a.task_id.cmp(&b.task_id))
        });
        let excess = result.items.len().saturating_sub(usize::from(tail));
        result.items.drain(..excess);
        Ok(result)
    }

    /// Orders tasks for a log read, running ones first, then the most
    /// recently updated by their Swarm version, and keeps the first
    /// [`MAX_LOG_TASKS`]: old tasks of a service's history come last.
    fn log_order(tasks: &mut Vec<Task>) {
        tasks.sort_by_key(|task| {
            let running = task
                .status
                .as_ref()
                .is_some_and(|status| status.state == Some(bollard::models::TaskState::RUNNING));
            let version = task.version.as_ref().and_then(|version| version.index);
            Reverse((running, version))
        });
        tasks.truncate(MAX_LOG_TASKS);
    }

    /// Reads one task's container output in `window`. A task without an ID
    /// or a known service is left out, truncating the result; one that never
    /// got a container has no output.
    async fn task_logs(
        &self,
        task: &Task,
        services: &HashMap<String, String>,
        window: LogWindow,
    ) -> Result<TaskLog, DockerError> {
        let mut log = TaskLog::new(window.budget);
        let Some(container) = Self::task_container(task) else {
            return Ok(log);
        };
        let (Some(task_id), Some(service)) = (
            task.id.as_ref(),
            task.service_id.as_ref().and_then(|id| services.get(id)),
        ) else {
            log.truncated = true;
            return Ok(log);
        };
        let stream = window.stream;
        let mut logs = self.client()?.logs(
            container,
            Some(
                LogsOptionsBuilder::default()
                    .stdout(stream != Some(piqueld_core::api::LogStream::Stderr))
                    .stderr(stream != Some(piqueld_core::api::LogStream::Stdout))
                    .timestamps(true)
                    .since(window.since)
                    .tail(&(u32::from(window.tail) + 1).to_string())
                    .build(),
            ),
        );
        while let Some(item) = logs.next().await {
            let Some(item) = item.found().or_request("read container logs")? else {
                log.truncated = true;
                break;
            };
            let source = match &item {
                LogOutput::StdOut { .. } => "stdout",
                LogOutput::StdErr { .. } => "stderr",
                _ => "console",
            };
            if stream.is_some_and(|filter| filter.as_str() != source) {
                continue;
            }
            for line in item.to_string().lines() {
                let (timestamp, message) = line.split_once(' ').unwrap_or(("", line));
                let message = LogRecord::clean_message(message);
                let size = message.len() + timestamp.len() + task_id.len() + service.len() + 128;
                log.push(
                    LogRecord {
                        service: service.clone(),
                        task_id: task_id.clone(),
                        timestamp: timestamp.into(),
                        stream: source.into(),
                        message,
                    },
                    size,
                );
            }
        }
        Ok(log)
    }

    /// Converts a look-back window in seconds into Docker's absolute `since`
    /// Unix timestamp, saturating at `i32::MAX`.
    fn log_since(window: u32) -> i32 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(u64::from(window));
        i32::try_from(timestamp).unwrap_or(i32::MAX)
    }

    /// The labels, with their values, of every managed service of
    /// `instance`'s environment `environment`. [`Self::owned_services`] lists
    /// services by them, and `FakeDocker` matches them as Docker does.
    pub(super) fn owner_labels(
        instance: &InstanceId,
        environment: &EnvironmentId,
    ) -> [(&'static str, String); 3] {
        [
            (INSTANCE_LABEL, instance.to_string()),
            (APPLICATION_LABEL, environment.to_string()),
            (MANAGED_LABEL, "true".to_owned()),
        ]
    }

    /// Maps service IDs to logical service names for the application's
    /// managed services, filtered to `service` when one is requested. Shared by
    /// log collection and exec task selection.
    pub(super) async fn owned_services(
        &self,
        instance: &InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
    ) -> Result<HashMap<String, String>, DockerError> {
        Ok(self
            .client()?
            .list_services(Some(
                ListServicesOptionsBuilder::default()
                    .filters(&Self::label_filter(
                        Self::owner_labels(instance, application)
                            .map(|(label, value)| format!("{label}={value}"))
                            .to_vec(),
                    ))
                    .build(),
            ))
            .await
            .or_request("list owned services")?
            .into_iter()
            .filter_map(|s| {
                let id = s.id?;
                let name = s.spec?.labels?.get(SERVICE_LABEL)?.clone();
                service
                    .is_none_or(|wanted| wanted == name)
                    .then_some((id, name))
            })
            .collect::<HashMap<_, _>>())
    }
}

#[cfg(test)]
mod tests;
