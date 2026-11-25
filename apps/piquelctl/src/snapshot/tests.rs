use super::*;

/// Runs `script` in `directory` with a fixed Git identity and date
/// (2000-01-01), returning its trimmed output.
fn sh(directory: &Path, script: &str) -> String {
    let output = std::process::Command::new("sh")
        .args(["-ec", script])
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "@946684800 +0000")
        .env("GIT_COMMITTER_DATE", "@946684800 +0000")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Every file of the worktree and its Git directory, with its hash: a
/// snapshot must leave refs, `HEAD`, the index, the stash, objects,
/// configuration, and the working tree as they were, and run no hook.
/// No Git command runs, since one could refresh the index itself.
fn state(work: &Path) -> String {
    sh(
        work,
        "find . -type f -print0 | sort -z | xargs -0 sha256sum",
    )
}

/// A clone of a two-commit repository, with an unpushed commit, a
/// modified file marked assume-unchanged, a staged, an untracked, an
/// ignored file, and a tracked one ignored since and edited. It splits
/// its index and has a hook that runs whenever the index changes.
fn fixture(root: &Path) -> PathBuf {
    sh(
        root,
        "git init -q -b main remote
         cd remote && echo one > app.toml && git add . && git commit -qm one
         echo two > app.toml && git commit -qam two && cd ..
         git clone -q remote work && cd work && git switch -qc feat/x
         git config core.splitIndex true
         printf '#!/bin/sh\ntouch hook-ran\n' > .git/hooks/post-index-change
         chmod +x .git/hooks/post-index-change
         printf 'ignored\n.env\n' > .gitignore && echo template > .env
         git add . && git add -f .env && git commit -qm unpushed && echo secret > .env
         echo changed > app.toml && git update-index --assume-unchanged app.toml
         echo staged > staged.txt && git add staged.txt
         echo untracked > untracked.txt && echo secret > ignored && rm hook-ran",
    );
    root.join("work")
}

/// The same worktree snapshots to the same commit, so a rerun can replay:
/// the commit takes `HEAD`'s dates, the fixture's 2000-01-01, not the
/// clock's, so a later snapshot matches too.
#[tokio::test]
async fn snapshots_of_an_unchanged_worktree_are_the_same_commit() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    let worktree = Worktree::at(&work).await.unwrap();
    let first = worktree.snapshot("feat/x", None).await.unwrap();
    let second = worktree.snapshot("feat/x", None).await.unwrap().commit;
    assert_eq!(first.commit, second);
    let show = ["show", "-s", "--format=%at %ct", &first.commit];
    let dates = Git::run(&mut first.staged(&show), "read the dates").await;
    assert_eq!(dates.unwrap(), "946684800 946684800");
}

#[tokio::test]
async fn files_not_in_head_that_likely_hold_credentials_are_found() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    // The fixture's `.env` is in `HEAD`, and `ignored` is ignored.
    let worktree = Worktree::at(&work).await.unwrap();
    assert_eq!(worktree.secret_files().await.unwrap(), Vec::<String>::new());
    // Staged but not committed counts, like untracked; source files,
    // templates and other names don't.
    sh(
        &work,
        "mkdir -p deploy && echo x > .env.local && git add .env.local \
         && echo key > deploy/server.PEM && echo key > id_ecdsa \
         && echo token > .git-credentials \
         && echo '{}' > application_default_credentials.json \
         && echo y > keys.txt && echo y > credentials.go && echo y > .env.example",
    );
    assert_eq!(
        worktree.secret_files().await.unwrap(),
        [
            ".env.local",
            ".git-credentials",
            "application_default_credentials.json",
            "deploy/server.PEM",
            "id_ecdsa",
        ]
    );
}

#[tokio::test]
async fn snapshots_include_untracked_files_and_leave_the_repository_untouched() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    let before = state(&work);
    let worktree = Worktree::at(&work).await.unwrap();
    assert_eq!(worktree.branch().await.unwrap().as_deref(), Some("feat/x"));
    let snapshot = worktree.snapshot("feat/x", None).await.unwrap();
    let thin = snapshot.bundle(History::Thin).await.unwrap();
    let complete = snapshot.bundle(History::Full).await.unwrap();
    assert_eq!(state(&work), before);
    // Read from the snapshot's own repository: the worktree's lacks it.
    let snapshot_git = format!(
        "git --git-dir={}",
        snapshot.directory.path().join("bundle.git").display()
    );
    let files = sh(
        &work,
        &format!("{snapshot_git} ls-tree -r --name-only {}", snapshot.commit),
    );
    assert_eq!(
        files,
        ".env\n.gitignore\napp.toml\nstaged.txt\nuntracked.txt"
    );
    assert_eq!(
        snapshot.uncommitted,
        ["app.toml", "staged.txt", "untracked.txt"]
    );
    // A tracked file ignored since is sent as committed.
    let env = sh(
        &work,
        &format!("{snapshot_git} show {}:.env", snapshot.commit),
    );
    assert_eq!(env, "template");
    // Edits hidden from `git status` by assume-unchanged are included.
    let manifest = sh(
        &work,
        &format!("{snapshot_git} show {}:app.toml", snapshot.commit),
    );
    assert_eq!(manifest, "changed");
    let parent = sh(
        &work,
        &format!("{snapshot_git} rev-parse {}^", snapshot.commit),
    );
    assert_eq!(parent, sh(&work, "git rev-parse HEAD"));

    // Thin against the nearest commit the remote has, which is not HEAD.
    let base = sh(&work, "git rev-parse origin/main");
    assert_eq!(thin.snapshot.base.as_deref(), Some(base.as_str()));
    assert_eq!(complete.snapshot.base, None);
    assert!(thin.length < complete.length);
    // Either applies to a clone of the remote, and carries the snapshot.
    for bundle in [&thin, &complete] {
        let heads = sh(
            &root.path().join("remote"),
            &format!(
                "git bundle verify -q {0} 2>/dev/null; git bundle list-heads {0}",
                bundle.path.display()
            ),
        );
        assert_eq!(heads, format!("{} refs/heads/snapshot", snapshot.commit));
    }
    // The objects of the snapshot were never written to the repository.
    let written = sh(
        &work,
        &format!("git cat-file -e {} && echo yes || echo no", snapshot.commit),
    );
    assert_eq!(written, "no");
}

/// Tracked files ignored since are sent as committed even when deleted
/// or staged as deleted, or named like pathspec magic, and their local
/// content does not count against the daemon's limits.
#[tokio::test]
async fn tracked_ignored_files_are_sent_as_committed() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    sh(
        &work,
        "echo committed > ':(exclude)x' && git --literal-pathspecs add ':(exclude)x'
         git commit -qm magic && printf ':(exclude)x\n' >> .gitignore
         head -c 1048576 /dev/zero > ':(exclude)x' && git rm -q --cached .env && rm .env",
    );
    let worktree = Worktree::at(&work).await.unwrap();
    let limits = SnapshotLimits {
        max_upload_bytes: 1 << 20,
        max_tree_bytes: 1 << 16,
        max_files: 100,
    };
    let snapshot = worktree.snapshot("feat/x", Some(&limits)).await.unwrap();
    snapshot.bundle(History::Thin).await.unwrap();
    // Its local content was never even staged.
    let local = sh(&work, "git hash-object -- ':(exclude)x'");
    let object = snapshot.objects().join(&local[..2]).join(&local[2..]);
    assert!(!object.exists());
    let snapshot_git = format!(
        "git --git-dir={}",
        snapshot.directory.path().join("bundle.git").display()
    );
    for (path, committed) in [(".env", "template"), (":(exclude)x", "committed")] {
        let content = sh(
            &work,
            &format!("{snapshot_git} show '{}:{path}'", snapshot.commit),
        );
        assert_eq!(content, committed, "{path}");
    }
}

/// Without remote-tracking branches, the whole history is bundled only
/// when asked for.
#[tokio::test]
async fn worktrees_without_remotes_need_the_whole_history_asked_for() {
    let root = tempfile::tempdir().unwrap();
    sh(
        root.path(),
        "git init -q -b main work && cd work && echo a > a && git add . && git commit -qm a",
    );
    let worktree = Worktree::at(&root.path().join("work")).await.unwrap();
    let snapshot = worktree.snapshot("main", None).await.unwrap();
    let Err(error) = snapshot.bundle(History::Thin).await else {
        panic!("the whole history must be asked for");
    };
    assert!(error.to_string().contains("--full-history"), "{error}");
    let bundle = snapshot.bundle(History::Full).await.unwrap();
    assert_eq!(bundle.snapshot.base, None);
    assert_eq!(bundle.commits, 1);
}

/// Files over the daemon's limits are refused before anything is
/// written, naming the largest; so is a bundle over its upload limit.
#[tokio::test]
async fn snapshots_over_the_daemon_limits_are_refused_early() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    sh(&work, "head -c 4096 /dev/zero > large.bin");
    let worktree = Worktree::at(&work).await.unwrap();
    let before = state(&work);
    let limits = SnapshotLimits {
        max_upload_bytes: 1 << 20,
        max_tree_bytes: 4096,
        max_files: 100,
    };
    let Err(error) = worktree.snapshot("feat/x", Some(&limits)).await else {
        panic!("files over the limit must be refused");
    };
    assert!(error.to_string().contains("large.bin (4.0 KiB)"), "{error}");
    assert_eq!(state(&work), before);
    let few = SnapshotLimits {
        max_files: 2,
        max_tree_bytes: 1 << 20,
        ..limits
    };
    assert!(worktree.snapshot("feat/x", Some(&few)).await.is_err());

    let roomy = SnapshotLimits {
        max_tree_bytes: 1 << 20,
        ..limits
    };
    let snapshot = worktree.snapshot("feat/x", Some(&roomy)).await.unwrap();
    let bundle = snapshot.bundle(History::Thin).await.unwrap();
    bundle
        .check_size(Some(&roomy))
        .expect("`check_size` should succeed");
    let small = SnapshotLimits {
        max_upload_bytes: bundle.length - 1,
        ..roomy
    };
    assert!(bundle.check_size(Some(&small)).is_err());
}

/// Detached heads have no branch, and submodules and nested
/// repositories are refused rather than half-supported.
#[tokio::test]
async fn detached_heads_have_no_branch_and_nested_repositories_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    sh(
        &work,
        "git switch -q --detach && git init -q nested && cd nested && echo n > n && git add . && git commit -qm n",
    );
    let worktree = Worktree::at(&work).await.unwrap();
    assert_eq!(worktree.branch().await.unwrap(), None);
    let Err(error) = worktree.snapshot("feat/x", None).await else {
        panic!("nested repositories must be refused");
    };
    assert!(error.to_string().contains("nested"), "{error}");
    assert!(Worktree::at(root.path()).await.is_err());
}

/// Files a configured filter driver would stage, such as LFS's, partial
/// clones, and SHA-256 repositories are refused before anything is
/// written; a filter attribute without a configured driver is harmless.
#[tokio::test]
async fn filtered_files_partial_clones_and_sha256_repositories_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let work = fixture(root.path());
    sh(
        &work,
        "echo '*.bin filter=cache' > .gitattributes && echo data > big.bin",
    );
    let worktree = Worktree::at(&work).await.unwrap();
    worktree
        .snapshot("feat/x", None)
        .await
        .expect("`snapshot` should succeed");
    sh(
        &work,
        "git config filter.cache.clean 'mkdir -p .git/filter-cache && cat'",
    );
    let before = state(&work);
    let Err(error) = worktree.snapshot("feat/x", None).await else {
        panic!("filtered files must be refused");
    };
    assert!(error.to_string().contains("big.bin"), "{error}");
    assert_eq!(state(&work), before);
    // A path starting with a space, listed first, is checked as is.
    sh(
        &work,
        "rm big.bin && echo '\" a.bin\" filter=cache' > .gitattributes && echo data > ' a.bin'",
    );
    let Err(error) = worktree.snapshot("feat/x", None).await else {
        panic!("filtered files must be refused");
    };
    assert!(error.to_string().contains(" a.bin"), "{error}");

    sh(
        root.path(),
        "git -C remote config uploadpack.allowFilter true && git clone -q --filter=blob:none \"file://$PWD/remote\" partial",
    );
    let worktree = Worktree::at(&root.path().join("partial")).await.unwrap();
    let Err(error) = worktree.snapshot("main", None).await else {
        panic!("partial clones must be refused");
    };
    assert!(error.to_string().contains("partial clone"), "{error}");

    sh(
        root.path(),
        "git init -q --object-format=sha256 -b main sha256 && cd sha256 && echo a > a && git add . && git commit -qm a",
    );
    let worktree = Worktree::at(&root.path().join("sha256")).await.unwrap();
    let Err(error) = worktree.snapshot("main", None).await else {
        panic!("SHA-256 repositories must be refused");
    };
    assert!(error.to_string().contains("sha256"), "{error}");
}
