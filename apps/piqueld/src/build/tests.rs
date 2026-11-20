use super::*;
use piqueld_core::{ApplicationId, api::BuildLogPage};
use std::fmt::Write as _;

#[test]
fn completion_retry_preserves_pending_terminal_result() {
    let reused = BuildOutcome {
        state: BuildState::Succeeded,
        image: Some("sha256:image".into()),
        reused_build: Some(7),
    };
    assert_eq!(Completion::Pending(reused.clone()).retry(), Some(reused));
    assert_eq!(
        Completion::Running.retry(),
        Some(BuildOutcome::without_image(BuildState::Interrupted))
    );
    assert_eq!(Completion::Persisted.retry(), None);
}

/// The head is cut and the tail trimmed on character boundaries, and the
/// tail follows a note of what was dropped between them.
#[test]
fn kept_output_is_cut_on_characters() {
    // 151 bytes of head, 64 for the note and 88 of tail.
    let mut kept = Kept::new(303);
    let text = "€".repeat(60);
    assert_eq!(kept.head(text.as_bytes()).len(), 150);
    kept.keep(&text.as_bytes()[150..], LogStream::Stdout);
    kept.keep("€".repeat(30).as_bytes(), LogStream::Stderr);
    assert_eq!(kept.head(b"x"), b"");
    let note = "\npiqueld: … 33 bytes omitted …\n";
    assert_eq!(
        kept.ending(),
        LogEnding {
            output: vec![
                (LogStream::Stderr, note.as_bytes().to_vec()),
                (LogStream::Stderr, "€".repeat(29).into_bytes()),
            ],
            truncated: true,
        }
    );
    kept.finished();
    assert_eq!(kept.ending(), LogEnding::default());
}

/// A cap too small for the omission note keeps the end of the output
/// without it, and still marks the log truncated.
#[test]
fn a_small_cap_keeps_a_tail_without_the_note() {
    // 64 bytes of head, 32 for the note and 32 of tail.
    let mut kept = Kept::new(128);
    let text = ["x".repeat(100), "y".repeat(100)].concat();
    assert_eq!(kept.head(text.as_bytes()).len(), 64);
    kept.keep(&text.as_bytes()[64..], LogStream::Stdout);
    assert_eq!(
        kept.ending(),
        LogEnding {
            output: vec![(LogStream::Stdout, "y".repeat(32).into_bytes())],
            truncated: true,
        }
    );
    assert_eq!(Kept::new(1).tail_max, 1);
}

/// A running build of a new application whose log keeps `cap` bytes.
async fn attempt(directory: &std::path::Path, cap: u32) -> BuildAttempt {
    let store = Store::open_unsynced(directory.join("db"))
        .await
        .unwrap()
        .with_build_history(piqueld_config::BuildHistoryConfig {
            log_max_bytes: cap,
            log_retention_days: 1,
        });
    let app = piqueld_core::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n\
         [[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx'",
    )
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let operation = store.save_application(&app, None, None).await.unwrap();
    let source = Source::Image {
        image: "nginx".into(),
    };
    BuildAttempt::start(
        Arc::new(store),
        &operation.environment_id,
        &operation.id,
        "web",
        &source,
        None,
    )
    .await
    .unwrap()
}

/// The text of a build's whole stored log, and whether it is truncated.
async fn stored(log: &BuildLog) -> (String, bool) {
    let mut text = String::new();
    let mut before = None;
    loop {
        let page: BuildLogPage = log.store.build_logs(log.id, before, None).await.unwrap();
        let chunks = page.items.iter().map(|chunk| chunk.text.as_str());
        text.insert_str(0, &chunks.collect::<String>());
        before = page.previous_offset;
        if before.is_none() {
            return (text, page.truncated);
        }
    }
}

/// Output past the cap keeps its start and its end, and takes no writer
/// turn between the head filling and the build finishing.
#[tokio::test]
async fn long_output_keeps_its_start_and_end() {
    let directory = tempfile::tempdir().unwrap();
    let attempt = attempt(directory.path(), 70_000).await;
    let log = attempt.log.clone();
    let text = lines();
    let (start, rest) = text.as_bytes().split_at(40_960);
    append(&log, start).await;
    let mut written = log.store.written();
    written.borrow_and_update();
    append(&log, rest).await;
    assert!(
        !written.has_changed().unwrap(),
        "the full head took a writer turn"
    );
    attempt.finish(BuildState::Failed, None).await.unwrap();
    assert_eq!(stored(&log).await, (kept(&text), true));
}

/// 220,000 bytes of numbered lines, past the tests' 70,000-byte cap.
fn lines() -> String {
    (0..20_000).fold(String::new(), |mut text, line| {
        writeln!(text, "line {line:05}").unwrap();
        text
    })
}

/// Appends `bytes` to `log` in 4 KiB pieces, as a command's reads arrive.
async fn append(log: &BuildLog, bytes: &[u8]) {
    for piece in bytes.chunks(4096) {
        log.append(piece, LogStream::Stdout).await.unwrap();
    }
}

/// What a 70,000-byte log keeps of `text`: its first 35,000 bytes, the
/// omission note and its last bytes.
fn kept(text: &str) -> String {
    let tail_max = 70_000 - 35_000 - Kept::NOTE_BYTES;
    let omitted = text.len() - 35_000 - tail_max;
    let note = format!("\npiqueld: … {omitted} bytes omitted …\n");
    [&text[..35_000], &note, &text[text.len() - tail_max..]].concat()
}

/// The database `directory`'s store uses, opened on its own.
async fn database(directory: &std::path::Path) -> sqlx::SqliteConnection {
    use sqlx::Connection as _;
    let url = format!("sqlite:{}", directory.join("db").display());
    sqlx::SqliteConnection::connect(&url).await.unwrap()
}

/// A finish that fails writes nothing and keeps the end of the output, so
/// retrying it stores the whole ending with the outcome.
#[tokio::test]
async fn a_failed_finish_keeps_the_ending_for_its_retry() {
    let directory = tempfile::tempdir().unwrap();
    let attempt = attempt(directory.path(), 70_000).await;
    let log = attempt.log.clone();
    let text = lines();
    append(&log, text.as_bytes()).await;
    let expected = kept(&text);
    let failed = BuildOutcome::without_image(BuildState::Failed);
    let mut database = database(directory.path()).await;
    let rename = |from: &str, to: &str| format!("ALTER TABLE {from} RENAME TO {to}");
    sqlx::query(&rename("build_log_chunks", "hidden"))
        .execute(&mut database)
        .await
        .unwrap();
    log.finish(&failed).await.unwrap_err();
    sqlx::query(&rename("hidden", "build_log_chunks"))
        .execute(&mut database)
        .await
        .unwrap();
    // Nothing of the failed finish was written.
    assert_eq!(stored(&log).await, (expected[..35_000].to_owned(), false));
    log.finish(&failed).await.unwrap();
    assert_eq!(stored(&log).await, (expected, true));
    let builds = log
        .store
        .builds(None, None, &piqueld_core::access::Scope::All, None, 1)
        .await
        .unwrap();
    assert_eq!(builds.items[0].state, BuildState::Failed);
}

/// A finish cancelled while it waits for the database is completed by
/// `Drop`, with the whole ending.
#[tokio::test]
async fn a_cancelled_finish_is_completed_with_its_ending() {
    let directory = tempfile::tempdir().unwrap();
    let attempt = attempt(directory.path(), 70_000).await;
    let log = attempt.log.clone();
    let text = lines();
    append(&log, text.as_bytes()).await;
    let expected = kept(&text);
    let mut database = database(directory.path()).await;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut database)
        .await
        .unwrap();
    let finish = attempt.finish(BuildState::Failed, None);
    tokio::time::timeout(std::time::Duration::from_millis(200), finish)
        .await
        .unwrap_err();
    sqlx::query("ROLLBACK")
        .execute(&mut database)
        .await
        .unwrap();
    let state = async || {
        log.store
            .builds(None, None, &piqueld_core::access::Scope::All, None, 1)
            .await
            .unwrap()
            .items[0]
            .state
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state().await == BuildState::Running {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state().await, BuildState::Failed);
    assert_eq!(stored(&log).await, (expected, true));
}
