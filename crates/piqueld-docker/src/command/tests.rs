//! Tests of command execution and output recording.
use super::*;

/// Failures report only the end of each stream, read to completion.
#[tokio::test]
async fn drains_both_streams_and_preserves_non_utf8_failures() {
    let error = LoggedCommand::run_recorded(Command::new("sh").args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; printf '\\377stdout tail'; printf '\\377stderr tail' >&2; exit 7"]), "fixture", None).await.expect_err("the command fails").to_string();
    assert!(error.contains("exit status: 7"), "{error}");
    assert!(error.contains("�stdout tail"));
    assert!(error.contains("�stderr tail"));
    assert!(error.len() < LoggedCommand::TAIL_BYTES * 2 + 200);
}

#[tokio::test]
async fn cancellation_kills_descendants_in_the_command_group() {
    for ending in ["wait", "exit 0"] {
        let directory = tempfile::tempdir().expect("the temp dir is created");
        let pidfile = directory.path().join("descendant.pid");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &pidfile,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .expect("the fifo is created");
        let path = pidfile.clone();
        let script = format!(r#"sh -c 'echo $$ > "$1"; exec sleep 60' child "$1" & {ending}"#);
        let command = tokio::spawn(async move {
            LoggedCommand::run_recorded(
                Command::new("sh").args([
                    "-c",
                    &script,
                    "parent",
                    path.to_str().expect("the path is utf-8"),
                ]),
                "cancellation fixture",
                None,
            )
            .await
        });
        let pid = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::fs::read_to_string(&pidfile),
        )
        .await
        .expect("the pid is written in time")
        .expect("the pid file is read");
        let pid = rustix::process::Pid::from_raw(pid.trim().parse().expect("the pid parses"))
            .expect("the pid is not zero");
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .expect("the process is running");
        command.abort();
        assert!(
            command
                .await
                .expect_err("the aborted task is cancelled")
                .is_cancelled()
        );
        let exited = tokio::task::spawn_blocking(move || {
            let mut fds = [rustix::event::PollFd::new(
                &pidfd,
                rustix::event::PollFlags::IN,
            )];
            let limit = rustix::event::Timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
            rustix::event::poll(&mut fds, Some(&limit))
        });
        assert_eq!(
            exited
                .await
                .expect("the poll task finishes")
                .expect("the poll succeeds"),
            1,
            "cancelled command descendant stopped"
        );
    }
}

/// Records each append a command makes to its log.
#[derive(Default)]
struct Appends(parking_lot::Mutex<Vec<Vec<u8>>>);

#[async_trait::async_trait]
impl OutputLog for Appends {
    async fn append(
        &self,
        bytes: &[u8],
        _stream: piqueld_core::api::LogStream,
    ) -> Result<(), LogError> {
        self.0.lock().push(bytes.to_vec());
        Ok(())
    }
}

/// A verbose command appends its output in a few large batches, not once
/// per 4 KiB read.
#[tokio::test]
async fn output_is_appended_in_batches() {
    let log = Appends::default();
    LoggedCommand::run_recorded(
        Command::new("sh").args(["-c", "head -c 1048576 /dev/zero"]),
        "fixture",
        Some(&log),
    )
    .await
    .expect("the command runs");
    let appends = log.0.into_inner();
    assert_eq!(appends.iter().map(Vec::len).sum::<usize>(), 1024 * 1024);
    assert!(appends.len() <= 17, "{} appends", appends.len());
}

/// Output followed by silence is appended after the batch delay, without
/// waiting for more output or the end of the stream.
#[tokio::test(start_paused = true)]
async fn quiet_output_is_appended_after_the_delay() {
    let log = Appends::default();
    let (mut writer, reader) = tokio::io::duplex(64);
    tokio::io::AsyncWriteExt::write_all(&mut writer, "ok€".as_bytes())
        .await
        .expect("the response is written");
    let read =
        LoggedCommand::tail_recorded(reader, Some(&log), piqueld_core::api::LogStream::Stdout);
    tokio::pin!(read);
    tokio::time::timeout(Batch::DELAY * 2, &mut read)
        .await
        .expect_err("the command times out");
    assert_eq!(*log.0.lock(), ["ok€".as_bytes()]);
    drop(writer);
    read.await.expect("the reader task finishes");
    assert_eq!(log.0.lock().len(), 1);
}
