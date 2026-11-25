use super::*;

/// Runs `script` in `directory` with a fixed Git identity, returning its
/// trimmed output.
fn sh(directory: &Path, script: &str) -> String {
    let output = std::process::Command::new("sh")
        .args(["-ec", script])
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A repository `remote` with two commits, and a clone whose snapshot
/// commit on top of them adds `local.txt`. Returns the base and the
/// snapshot commits.
fn fixture(root: &Path) -> (String, String) {
    sh(
        root,
        "git init -q -b main remote
         cd remote && echo one > app.toml && git add . && git commit -qm one
         echo two > app.toml && git commit -qam two && cd ..
         git clone -q remote work && cd work && echo local > local.txt && git add .
         git commit -qm snapshot && git update-ref refs/heads/snapshot HEAD",
    );
    (
        sh(&root.join("remote"), "git rev-parse HEAD"),
        sh(&root.join("work"), "git rev-parse HEAD"),
    )
}

/// The bytes of a bundle of `work`'s snapshot ref, with `arguments`.
fn bundle(root: &Path, arguments: &str) -> Vec<u8> {
    sh(
        &root.join("work"),
        &format!("git bundle create -q ../x.bundle refs/heads/snapshot {arguments}"),
    );
    std::fs::read(root.join("x.bundle")).unwrap()
}

fn body(bytes: &[u8]) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::iter(
        bytes
            .chunks(1000)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>(),
    )
}

/// Checkouts within `max_bytes`; only their limit is used.
fn checkouts(max_bytes: u64) -> Checkouts {
    Checkouts::new(
        PathBuf::from("unused"),
        std::num::NonZeroU64::new(max_bytes).unwrap(),
    )
}

/// Snapshots kept in `root/snapshots`.
async fn snapshots(root: &Path) -> Snapshots {
    Snapshots::open(root.join("snapshots")).await.unwrap()
}

async fn receive<'a>(
    snapshots: &'a Snapshots,
    bytes: Vec<u8>,
    limit: u64,
    url: &Path,
) -> Result<Received<'a>, SnapshotError> {
    let url = url.display().to_string();
    snapshots
        .receive(body(&bytes), (None, limit), &url, &checkouts(1 << 30))
        .await
}

/// A thin bundle needs its base, which is fetched from the repository;
/// a complete one needs nothing. Either is checked out from its own
/// repository once kept, and holds its upload permit until then, so
/// uploads waiting to be accepted never pile up on disk.
#[tokio::test]
async fn receives_thin_and_complete_bundles() {
    let root = tempfile::tempdir().unwrap();
    let (base, commit) = fixture(root.path());
    let remote = root.path().join("remote");
    let snapshots = snapshots(root.path()).await;
    let thin = bundle(root.path(), &format!("^{base}"));
    let complete = bundle(root.path(), "");
    assert!(thin.len() < complete.len());
    for (bytes, expected) in [(thin, Some(base)), (complete, None)] {
        let received = receive(&snapshots, bytes, 1 << 20, &remote).await.unwrap();
        assert_eq!((&received.commit, &received.base), (&commit, &expected));
        let permits = || snapshots.receiving.available_permits();
        assert_eq!(permits(), Snapshots::RECEIVING - 1);
        let commit = received.commit.clone();
        let kept = snapshots
            .keep(received, async { Ok::<_, SnapshotError>(()) })
            .await;
        kept.expect("`kept` should succeed");
        assert_eq!(permits(), Snapshots::RECEIVING);
        assert!(snapshots.path(&commit).exists());
    }
    let source = snapshots.source(&remote.display().to_string(), &commit);
    let checkouts = Checkouts::open(root.path().join("checkouts"))
        .await
        .unwrap();
    let checkout = checkouts.checkout(&source.repository, None).await.unwrap();
    assert_eq!(checkout.commit, commit);
    assert_eq!(
        tokio::fs::read_to_string(checkout.path("local.txt").await.unwrap())
            .await
            .unwrap(),
        "local\n"
    );
    let pinned = GitRepository {
        url: remote.display().to_string(),
        branch: "main".into(),
        commit: Some(commit.clone()),
    };
    let other = GitRepository {
        url: "https://example.com/other.git".into(),
        ..pinned.clone()
    };
    assert_eq!(
        SnapshotSource::checkout(Some(&source), &pinned),
        &source.repository
    );
    assert_eq!(SnapshotSource::checkout(Some(&source), &other), &other);
    let staged = std::fs::read_dir(root.path().join("snapshots/staging")).unwrap();
    assert_eq!(staged.count(), 0);
}

/// Oversized, malformed, and corrupted bundles, and bases the repository
/// lacks, are refused with their own errors, and a snapshot whose
/// deployment is refused is not kept.
#[tokio::test]
async fn refuses_oversized_invalid_and_unfetchable_bundles() {
    let root = tempfile::tempdir().unwrap();
    let (base, _) = fixture(root.path());
    let remote = root.path().join("remote");
    let snapshots = snapshots(root.path()).await;
    let thin = bundle(root.path(), &format!("^{base}"));
    let length = thin.len() as u64;

    let announced = snapshots
        .receive(
            body(&thin),
            (Some(length), length - 1),
            "unused",
            &checkouts(1 << 30),
        )
        .await;
    assert!(matches!(announced, Err(SnapshotError::TooLarge { .. })));
    let streamed = receive(&snapshots, thin.clone(), length - 1, &remote).await;
    assert!(matches!(streamed, Err(SnapshotError::TooLarge { .. })));

    let garbage = receive(&snapshots, b"not a bundle\n\n".to_vec(), 1 << 20, &remote).await;
    assert!(matches!(garbage, Err(SnapshotError::Invalid(_))));
    let truncated = receive(
        &snapshots,
        thin[..thin.len() - 40].to_vec(),
        1 << 20,
        &remote,
    );
    assert!(matches!(truncated.await, Err(SnapshotError::Invalid(_))));

    let received = receive(&snapshots, thin.clone(), 1 << 20, &remote)
        .await
        .unwrap();
    let commit = received.commit.clone();
    let refused = snapshots
        .keep(received, async {
            Err::<(), _>(SnapshotError::Invalid("refused"))
        })
        .await;
    assert!(refused.is_err());
    assert!(!snapshots.path(&commit).exists());

    let url = remote.display().to_string();
    let expands = snapshots
        .receive(body(&thin), (None, 1 << 20), &url, &checkouts(9))
        .await;
    assert!(matches!(
        expands,
        Err(SnapshotError::TreeTooLarge { limit: 9 })
    ));

    sh(root.path(), "git init -q -b main empty");
    let unfetchable = receive(&snapshots, thin, 1 << 20, &root.path().join("empty")).await;
    assert!(
        matches!(unfetchable, Err(SnapshotError::BaseUnavailable { prerequisites }) if prerequisites == [base])
    );
}

/// A thin bundle of a history that merged a branch forked before its
/// base needs the base and that fork point, and its base is the one
/// they share. Prerequisites that are not one history are refused.
#[tokio::test]
async fn thin_bundles_of_merges_need_their_base_and_its_ancestors() {
    let root = tempfile::tempdir().unwrap();
    let base = sh(
        root.path(),
        "git init -q -b main remote && cd remote
         echo a > a && git add . && git commit -qm a && git switch -qc side
         echo s > s && git add . && git commit -qm s && git switch -q main
         echo b > a && git commit -qam b && git rev-parse HEAD",
    );
    let commit = sh(
        root.path(),
        "git clone -q remote work && cd work && git merge -q --no-edit origin/side
         git update-ref refs/heads/snapshot HEAD && git rev-parse HEAD",
    );
    let remote = root.path().join("remote");
    let snapshots = snapshots(root.path()).await;
    let merged = bundle(root.path(), &format!("^{base}"));
    let received = receive(&snapshots, merged, 1 << 20, &remote).await.unwrap();
    assert_eq!((received.commit, received.base), (commit, Some(base)));
    let unrelated = bundle(root.path(), "^HEAD^1 ^HEAD^2");
    let refused = receive(&snapshots, unrelated, 1 << 20, &remote).await;
    assert!(matches!(refused, Err(SnapshotError::Invalid(_))));
}

/// A body that stops sending gives up its permit.
#[tokio::test(start_paused = true)]
async fn stalled_uploads_time_out() {
    let root = tempfile::tempdir().unwrap();
    let snapshots = snapshots(root.path()).await;
    let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    let received = snapshots
        .receive(stalled, (None, 1 << 20), "unused", &checkouts(1 << 30))
        .await;
    assert!(matches!(received, Err(SnapshotError::TimedOut)));
    assert_eq!(
        snapshots.receiving.available_permits(),
        Snapshots::RECEIVING
    );
}

#[test]
fn headers_name_one_snapshot_and_its_prerequisites() {
    let (a, b) = ("a".repeat(40), "b".repeat(40));
    let parse = |lines: &[String]| BundleHeader::parse(lines);
    let header = |lines: &[&str]| {
        lines
            .iter()
            .map(|line| (*line).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        parse(&header(&[
            "# v3 git bundle",
            "@object-format=sha1",
            &format!("-{b} subject"),
            &format!("-{a}"),
            &format!("{b} refs/heads/snapshot"),
        ])),
        Some(BundleHeader {
            reference: "refs/heads/snapshot".into(),
            commit: b.clone(),
            prerequisites: vec![a.clone(), b.clone()],
        })
    );
    let mut many = vec!["# v2 git bundle".to_owned()];
    many.extend((0..=BundleHeader::PREREQUISITES).map(|n| format!("-{n:040x}")));
    many.push(format!("{b} refs/heads/snapshot"));
    assert_eq!(parse(&many), None);
    for invalid in [
        header(&["# v4 git bundle", &format!("{b} refs/heads/snapshot")]),
        header(&[
            "# v2 git bundle",
            "@object-format=sha1",
            &format!("{b} refs/heads/x"),
        ]),
        header(&[
            "# v3 git bundle",
            "@filter=blob:none",
            &format!("{b} refs/heads/x"),
        ]),
        header(&[
            "# v2 git bundle",
            &format!("{b} refs/heads/x"),
            &format!("{a} refs/heads/y"),
        ]),
        header(&[
            "# v2 git bundle",
            &format!("-{a}"),
            &format!("-{a}"),
            &format!("{b} refs/x"),
        ]),
        header(&["# v2 git bundle", &format!("{b} HEAD")]),
        header(&["# v2 git bundle", &format!("{b} refs/heads/a:refs/heads/b")]),
        header(&["# v2 git bundle", &format!("{b} refs/heads/../x")]),
        header(&["# v2 git bundle", "1234 refs/heads/x"]),
        header(&["# v2 git bundle"]),
    ] {
        assert_eq!(parse(&invalid), None, "{invalid:?}");
    }
}

/// Pruning keeps exactly the snapshots still needed.
#[tokio::test]
async fn prune_keeps_only_needed_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let snapshots = snapshots(root.path()).await;
    let (kept, removed) = ("a".repeat(40), "b".repeat(40));
    for commit in [&kept, &removed] {
        std::fs::create_dir(snapshots.path(commit)).unwrap();
    }
    let needed = vec![kept.clone()];
    snapshots.prune(async { Ok(needed) }).await.unwrap();
    assert!(snapshots.path(&kept).exists());
    assert!(!snapshots.path(&removed).exists());
    assert!(root.path().join("snapshots/staging").exists());
}
