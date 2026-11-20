use super::*;

/// Task `index` at Swarm version `index`, running or shut down.
fn task(index: u64, running: bool) -> Task {
    Task {
        id: Some(format!("task-{index}")),
        version: Some(bollard::models::ObjectVersion { index: Some(index) }),
        status: Some(bollard::models::TaskStatus {
            state: Some(if running {
                bollard::models::TaskState::RUNNING
            } else {
                bollard::models::TaskState::SHUTDOWN
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Running tasks come first, then the newest, and old history past the
/// limit is left out rather than the running task.
#[test]
fn logs_read_running_then_newest_tasks() {
    let mut tasks = (0..300).map(|index| task(index, index == 5)).collect();
    BollardDocker::log_order(&mut tasks);
    let ids = tasks
        .iter()
        .map(|task| task.id.as_deref().expect("an ID"))
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), MAX_LOG_TASKS);
    assert_eq!(ids[..3], ["task-5", "task-299", "task-298"]);
    assert_eq!(ids.last(), Some(&"task-45"));
}

/// A task past its budget keeps its newest output.
#[test]
fn a_task_keeps_its_newest_output_within_its_budget() {
    let record = |message: &str| LogRecord {
        service: "web".into(),
        task_id: "task-1".into(),
        timestamp: String::new(),
        stream: "stdout".into(),
        message: message.into(),
    };
    let mut log = TaskLog::new(300);
    for message in ["first", "second", "third"] {
        log.push(record(message), 128);
    }
    let kept = log
        .records
        .iter()
        .map(|(record, _)| record.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(kept, ["second", "third"]);
    assert!(log.truncated);
}
