use super::*;
use piqueld_core::manifest::ApplicationTemplate;

/// A build that succeeded and produced `image`.
fn succeeded(image: &str) -> BuildOutcome {
    BuildOutcome {
        state: BuildState::Succeeded,
        image: Some(image.into()),
        reused_build: None,
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    app: piqueld_core::NormalizedApplication,
    /// The application's production environment.
    environment: EnvironmentId,
    operation: String,
    id: i64,
}
impl Fixture {
    /// Saves the prebuilt application and starts a build of its `web`
    /// service, keeping 70,000 bytes of output for a day.
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open_unsynced(temp.path().join("state.db"))
            .await
            .unwrap()
            .with_build_history(piqueld_core::BuildHistoryConfig {
                log_max_bytes: 70_000,
                log_retention_days: 1,
            });
        let app = crate::testing::prebuilt("app-build-test");
        let operation = store
            .save_application(&ApplicationTemplate::from(&app), None, None)
            .await
            .unwrap();
        let environment = store.production(app.id()).await;
        let mut fixture = Self {
            _temp: temp,
            store,
            app,
            environment,
            operation: operation.id,
            id: 0,
        };
        fixture.id = fixture.start().await;
        fixture
    }

    /// Starts another build of the application's `web` service.
    async fn start(&self) -> i64 {
        self.store
            .start_build(
                &self.environment,
                &self.operation,
                "web",
                &self.app.spec().services[0].source,
                None,
            )
            .await
            .unwrap()
    }
}

/// A Git source on `branch`, built with the build argument `MODE=mode`.
fn git_source(branch: &str, mode: &str) -> Source {
    Source::Git {
        repository: piqueld_core::manifest::SourceRepository::Git(
            piqueld_core::manifest::GitRepository {
                url: "https://example.com/notes.git".into(),
                branch: branch.into(),
                commit: None,
            },
        ),
        build: piqueld_core::manifest::ValidatedBuild::Docker {
            dockerfile: "Dockerfile".into(),
            context: ".".into(),
            args: [("MODE".into(), mode.into())].into(),
            target: None,
        },
    }
}

/// A full commit hash or an image ID's hexadecimal part, of `digit`s.
fn hex(digit: char, length: usize) -> String {
    digit.to_string().repeat(length)
}

/// Records a build of `source` at commit `commit`s for `environment`,
/// ended with `outcome`.
async fn record(
    fixture: &Fixture,
    environment: &EnvironmentId,
    source: &Source,
    commit: char,
    outcome: BuildOutcome,
) -> i64 {
    let store = &fixture.store;
    let id = store
        .start_build(environment, &fixture.operation, "web", source, None)
        .await
        .unwrap();
    store.build_commit(id, &hex(commit, 40)).await.unwrap();
    store
        .finish_build(id, &outcome, &LogEnding::default())
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn only_the_applications_builds_of_the_same_commit_and_settings_are_reusable() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    let image = |digit| format!("sha256:{}", hex(digit, 64));
    let release = git_source("main", "release");
    let ours = &fixture.environment;
    let built = record(&fixture, ours, &release, 'c', succeeded(&image('a'))).await;
    // Other build settings, another commit, a failed build, a record
    // that no longer decodes, and another application's build of the
    // same inputs.
    let debug = git_source("main", "debug");
    record(&fixture, ours, &debug, 'c', succeeded(&image('b'))).await;
    record(&fixture, ours, &release, 'd', succeeded(&image('b'))).await;
    let failed = BuildOutcome::without_image(BuildState::Failed);
    record(&fixture, ours, &release, 'c', failed).await;
    record(&fixture, ours, &release, 'c', succeeded("gone")).await;
    let other = crate::testing::template("other", "[spec]");
    let theirs = store
        .saved_production(&store.save_template(other, false).await)
        .await;
    record(&fixture, &theirs, &release, 'c', succeeded(&image('b'))).await;

    // The branch a commit was built from doesn't matter.
    let preview = git_source("feat/notes", "release");
    let reusable = |source| async move {
        store
            .reusable_builds(ours, source, &hex('c', 40))
            .await
            .unwrap()
    };
    let original = ReusableBuild {
        id: built,
        image: BuiltImage::parse(image('a')).unwrap(),
    };
    assert_eq!(reusable(&preview).await, std::slice::from_ref(&original));

    // A build that reused the image names the build that produced it.
    let reused = BuildOutcome {
        reused_build: Some(built),
        ..succeeded(&image('a'))
    };
    record(&fixture, ours, &preview, 'c', reused).await;
    assert_eq!(reusable(&release).await, [original.clone(), original]);
}

#[tokio::test]
async fn build_metadata_obeys_the_writer_gate() {
    let Fixture {
        _temp, store, id, ..
    } = Fixture::new().await;
    let writer = store.writers.lock().await;
    let blocked = std::time::Duration::from_millis(50);
    let failed = BuildOutcome::without_image(BuildState::Failed);
    let ending = LogEnding::default();
    let (commit, finish, recover) = tokio::join!(
        tokio::time::timeout(blocked, store.build_commit(id, "commit")),
        tokio::time::timeout(blocked, store.finish_build(id, &failed, &ending)),
        tokio::time::timeout(blocked, store.recover_builds()),
    );
    assert!(commit.is_err(), "commit metadata bypassed the writer gate");
    assert!(finish.is_err(), "completion bypassed the writer gate");
    assert!(recover.is_err(), "recovery bypassed the writer gate");
    drop(writer);
    store.build_commit(id, "commit").await.unwrap();
    store
        .finish_build(
            id,
            &BuildOutcome::without_image(BuildState::Failed),
            &LogEnding::default(),
        )
        .await
        .unwrap();
    store.recover_builds().await.unwrap();
}

#[tokio::test]
async fn output_survives_concurrent_metadata_writes_for_another_build() {
    let fixture = Fixture::new().await;
    let other = fixture.start().await;
    let Fixture {
        store,
        environment,
        id,
        ..
    } = &fixture;
    let id = *id;
    let output = async {
        for _ in 0..8 {
            store
                .append_build_log(id, &[b'a'; 4096], LogStream::Stdout)
                .await?;
        }
        Ok::<_, StoreError>(())
    };
    let metadata = async {
        for _ in 0..8 {
            store.build_commit(other, "commit").await?;
        }
        store
            .finish_build(other, &succeeded("sha256:fixture"), &LogEnding::default())
            .await
    };
    tokio::try_join!(output, metadata).unwrap();
    let logs = store.build_logs(id, None, None).await.unwrap();
    assert_eq!(logs.items.len(), 8);
    for (index, chunk) in logs.items.iter().enumerate() {
        assert_eq!(chunk.offset, i64::try_from(index * 4096).unwrap());
        assert_eq!(chunk.text, "a".repeat(4096));
    }
    let records = store
        .builds(None, Some(environment), &Scope::All, None, 50)
        .await
        .unwrap();
    assert_eq!(records.items[0].state, BuildState::Succeeded);
    assert_eq!(records.items[0].commit.as_deref(), Some("commit"));
    assert_eq!(records.items[1].log_bytes, 32768);
}

#[tokio::test]
async fn build_pages_filter_before_applying_the_limit() {
    let fixture = Fixture::new().await;
    let Fixture {
        store,
        app,
        environment,
        id,
        ..
    } = &fixture;
    let mut manifest = app.to_manifest();
    manifest.metadata.name = "other".into();
    let other = manifest
        .validate()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-other").unwrap());
    let other_operation = store
        .save_application(&ApplicationTemplate::from(&other), None, Some(0))
        .await
        .unwrap();
    let latest = fixture.start().await;
    for _ in 0..3 {
        store
            .start_build(
                &store.production(other.id()).await,
                &other_operation.id,
                "web",
                &other.spec().services[0].source,
                None,
            )
            .await
            .unwrap();
    }
    let first = store
        .builds(None, Some(environment), &Scope::All, None, 1)
        .await
        .unwrap();
    assert_eq!(first.items[0].id, latest);
    let cursor = first.next_cursor.as_deref();
    let second = store
        .builds(None, Some(environment), &Scope::All, cursor, 1)
        .await
        .unwrap();
    assert_eq!(second.items[0].id, *id);
    assert!(second.next_cursor.is_none());
    let global = store
        .builds(None, None, &Scope::All, None, 1)
        .await
        .unwrap();
    assert_eq!(
        global.items[0].environment_id,
        other_operation.environment_id.as_str()
    );
    let application = store
        .builds(Some(app.id()), None, &Scope::All, None, 1)
        .await
        .unwrap();
    assert_eq!(application.items[0].id, latest);
    let absent = EnvironmentId::parse("env-absent").unwrap();
    assert!(
        store
            .builds(None, Some(&absent), &Scope::All, None, 1)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn output_is_bounded_paged_and_expired_without_losing_metadata() {
    let fixture = Fixture::new().await;
    let Fixture {
        store,
        environment,
        id,
        ..
    } = &fixture;
    let id = *id;
    assert_eq!(
        store
            .append_build_log(id, &vec![b'a'; 80_000], LogStream::Stdout)
            .await
            .unwrap(),
        LogAppend::Closed
    );
    let first = store.build_logs(id, None, None).await.unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|chunk| chunk.text.len())
            .sum::<usize>(),
        61808
    );
    assert!(first.truncated);
    let second = store
        .build_logs(id, first.previous_offset, None)
        .await
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(|chunk| chunk.text.len())
            .sum::<usize>(),
        8192
    );
    assert!(second.previous_offset.is_none());
    store
        .finish_build(id, &succeeded("sha256:fixture"), &LogEnding::default())
        .await
        .unwrap();
    assert_eq!(
        store
            .append_build_log(id, b"late", LogStream::Stdout)
            .await
            .unwrap(),
        LogAppend::Closed
    );
    assert_eq!(
        store
            .builds(None, Some(environment), &Scope::All, None, 50)
            .await
            .unwrap()
            .items[0]
            .log_bytes,
        70_000
    );
    sqlx::query!("UPDATE builds SET finished_at_ms=0 WHERE id=?1", id)
        .execute(&store.pool)
        .await
        .unwrap();
    store.prune_build_logs().await.unwrap();
    assert!(store.build_logs(id, None, None).await.unwrap().expired);
    let records = store
        .builds(None, None, &Scope::All, None, 50)
        .await
        .unwrap();
    assert_eq!(records.items.len(), 1);
    assert_eq!(records.items[0].state, BuildState::Succeeded);
    let interrupted = fixture.start().await;
    store.recover_builds().await.unwrap();
    let page = store
        .builds(None, None, &Scope::All, None, 1)
        .await
        .unwrap();
    assert_eq!(page.items[0].id, interrupted);
    assert_eq!(page.items[0].state, BuildState::Interrupted);
    assert_eq!(
        store
            .builds(None, None, &Scope::All, page.next_cursor.as_deref(), 1)
            .await
            .unwrap()
            .items[0]
            .id,
        id
    );
}
#[tokio::test]
async fn output_filters_before_paging_backwards() {
    let fixture = Fixture::new().await;
    let (store, id) = (&fixture.store, fixture.id);
    store
        .append_build_log(id, &vec![b'a'; 80_000], LogStream::Stdout)
        .await
        .unwrap();
    let newest = store
        .build_logs(id, None, Some(LogStream::Stdout))
        .await
        .unwrap();
    assert_eq!(newest.items.len(), 16);
    assert!(
        newest
            .items
            .iter()
            .all(|chunk| chunk.stream == LogStream::Stdout && chunk.timestamp_ms > 0)
    );
    let older = store
        .build_logs(id, newest.previous_offset, Some(LogStream::Stdout))
        .await
        .unwrap();
    assert_eq!(
        older
            .items
            .iter()
            .chain(&newest.items)
            .map(|chunk| chunk.text.len())
            .sum::<usize>(),
        70_000
    );
    assert!(older.previous_offset.is_none());
    assert!(older.items.last().unwrap().offset < newest.items[0].offset);
    assert!(
        store
            .build_logs(id, None, Some(LogStream::Stderr))
            .await
            .unwrap()
            .items
            .is_empty()
    );

    // A sparse stream must be filtered before the 16-chunk page limit.
    let id = fixture.start().await;
    store
        .append_build_log(id, b"error output", LogStream::Stderr)
        .await
        .unwrap();
    store
        .append_build_log(id, &vec![b'x'; 68_000], LogStream::Stdout)
        .await
        .unwrap();
    let errors = store
        .build_logs(id, None, Some(LogStream::Stderr))
        .await
        .unwrap();
    assert_eq!(errors.items[0].text, "error output");
    assert!(errors.previous_offset.is_none());
}
#[tokio::test]
async fn structured_chunks_preserve_multibyte_output() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    for (repeats, retained, truncated) in [(3000, 9000, false), (25000, 69999, true)] {
        // Each case writes a build of its own.
        let id = fixture.start().await;
        let text = "€".repeat(repeats);
        store
            .append_build_log(id, text.as_bytes(), LogStream::Stdout)
            .await
            .unwrap();
        if truncated {
            // The unused byte at the cap must not admit output after a gap.
            store
                .append_build_log(id, b"x", LogStream::Stdout)
                .await
                .unwrap();
        }
        let mut output = String::new();
        let mut before = None;
        loop {
            let page = store.build_logs(id, before, None).await.unwrap();
            assert_eq!(page.truncated, truncated);
            let text = page
                .items
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>();
            output.insert_str(0, &text);
            before = page.previous_offset;
            if before.is_none() {
                break;
            }
        }
        assert_eq!(output, text[..retained]);
        assert!(!output.contains('�'));
    }
}
