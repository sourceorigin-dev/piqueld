//! Tests of command execution and output recording.
use super::*;

/// Failures report only the end of each stream, read to completion.
#[tokio::test]
async fn drains_both_streams_and_preserves_non_utf8_failures() {
    let error = LoggedCommand::run_recorded(Command::new("sh").args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; printf '\\377stdout tail'; printf '\\377stderr tail' >&2; exit 7"]), "fixture", None).await.unwrap_err().to_string();
    assert!(error.contains("exit status: 7"), "{error}");
    assert!(error.contains("�stdout tail"));
    assert!(error.contains("�stderr tail"));
    assert!(error.len() < LoggedCommand::TAIL_BYTES * 2 + 200);
}

#[tokio::test]
async fn cancellation_kills_descendants_in_the_command_group() {
    // Exercise cancellation with both a live leader and an exited leader
    // whose descendant still owns its output streams.
    for ending in ["wait", "exit 0"] {
        let directory = tempfile::tempdir().unwrap();
        let pidfile = directory.path().join("descendant.pid");
        let path = pidfile.clone();
        let script = format!(r#"sh -c 'echo $$ > "$1"; exec sleep 60' child "$1" & {ending}"#);
        let command = tokio::spawn(async move {
            LoggedCommand::run_recorded(
                Command::new("sh").args(["-c", &script, "parent", path.to_str().unwrap()]),
                "cancellation fixture",
                None,
            )
            .await
        });
        let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&pidfile).await
                    && let Ok(pid) = contents.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        command.abort();
        assert!(command.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                // Orphaned descendants can briefly be zombies until PID 1 reaps
                // them. They cannot continue work or retain build pipes/slots.
                let status = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await;
                if status
                    .as_ref()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    || status.as_ref().is_ok_and(|status| status.contains(") Z "))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled command descendant stopped");
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
    ) -> anyhow::Result<()> {
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
    .unwrap();
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
        .unwrap();
    let read =
        LoggedCommand::tail_recorded(reader, Some(&log), piqueld_core::api::LogStream::Stdout);
    tokio::pin!(read);
    // The stream stays open: only the delay can append it.
    tokio::time::timeout(Batch::DELAY * 2, &mut read)
        .await
        .unwrap_err();
    assert_eq!(*log.0.lock(), ["ok€".as_bytes()]);
    drop(writer);
    read.await.unwrap();
    assert_eq!(log.0.lock().len(), 1);
}
