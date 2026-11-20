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
    // 100 bytes of head and 37 of tail.
    let mut kept = Kept::new(201);
    let text = "€".repeat(40);
    assert_eq!(kept.head(text.as_bytes()).len(), 99);
    kept.keep(&text.as_bytes()[99..], LogStream::Stdout);
    kept.keep("€".repeat(20).as_bytes(), LogStream::Stderr);
    assert_eq!(kept.head(b"x"), b"");
    let note = "\npiqueld: … 45 bytes omitted …\n";
    assert_eq!(
        kept.close(),
        (
            true,
            vec![
                (LogStream::Stderr, note.as_bytes().to_vec()),
                (LogStream::Stderr, "€".repeat(12).into_bytes()),
            ]
        )
    );
    assert_eq!(kept.close(), (false, Vec::new()));
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
    let text = (0..20_000).fold(String::new(), |mut text, line| {
        writeln!(text, "line {line:05}").unwrap();
        text
    });
    let (start, rest) = text.as_bytes().split_at(40_960);
    for piece in start.chunks(4096) {
        log.append(piece, LogStream::Stdout).await.unwrap();
    }
    let mut written = log.store.written();
    written.borrow_and_update();
    for piece in rest.chunks(4096) {
        log.append(piece, LogStream::Stdout).await.unwrap();
    }
    assert!(
        !written.has_changed().unwrap(),
        "the full head took a writer turn"
    );
    attempt.finish(BuildState::Failed, None).await.unwrap();
    let (stored, truncated) = stored(&log).await;
    assert!(truncated);
    let tail_max = 70_000 - 35_000 - Kept::NOTE_BYTES;
    let omitted = text.len() - 35_000 - tail_max;
    let note = format!("\npiqueld: … {omitted} bytes omitted …\n");
    assert_eq!(
        stored,
        [&text[..35_000], &note, &text[text.len() - tail_max..]].concat()
    );
}
